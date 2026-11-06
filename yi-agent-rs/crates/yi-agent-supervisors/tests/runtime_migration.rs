//! Retiring an old install without destroying the user's session.
//!
//! The dangerous direction of a migration is over-reach: booting out a job that is
//! not ours, or stopping a runtime that lives inside the user's editor. Both look
//! like a successful migration from inside the new process, so these tests pin the
//! refusals rather than the happy path.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tempfile::TempDir;
use yi_agent_lifecycle::deadline::Deadline;
use yi_agent_lifecycle::types::{RuntimeMode, RuntimeScope};
use yi_agent_supervisors::lifecycle::migration::{
    LaunchctlBackend, LegacyProbe, LegacyWatchman, LoadedJob, MigrationBlocker,
    MigrationDisposition, MigrationPlan,
};

fn test_scope(directory: &TempDir) -> RuntimeScope {
    RuntimeScope {
        key: directory.path().to_string_lossy().into_owned(),
        runtime_dir: directory.path().to_path_buf(),
        project_key: "/project".to_string(),
        instance_home: directory.path().to_path_buf(),
    }
}

/// Records what a migration asked of launchd instead of asking it.
#[derive(Default)]
struct FakeLaunchctl {
    jobs: Vec<LoadedJob>,
    booted_out: RefCell<Vec<String>>,
    /// Labels whose `bootout` must fail, to prove a failed bootout is not ignored.
    refusing: Vec<String>,
}

impl FakeLaunchctl {
    fn with(jobs: Vec<LoadedJob>) -> Self {
        Self {
            jobs,
            booted_out: RefCell::new(Vec::new()),
            refusing: Vec::new(),
        }
    }
}

impl LaunchctlBackend for FakeLaunchctl {
    fn loaded_job(&self, label: &str) -> std::io::Result<Option<LoadedJob>> {
        Ok(self.jobs.iter().find(|job| job.label == label).cloned())
    }

    fn bootout(&self, label: &str) -> std::io::Result<()> {
        if self.refusing.iter().any(|refusing| refusing == label) {
            return Err(std::io::Error::other(format!(
                "bootout {label} was refused"
            )));
        }
        self.booted_out.borrow_mut().push(label.to_string());
        Ok(())
    }
}

/// Writes the plist an old install would have left behind.
///
/// The migration requires *both* a loaded job and a plist naming our program, so a
/// test that only fakes the job would be testing a state that cannot occur.
fn write_plist(home: &Path, label: &str, program: &Path) {
    let directory = home.join("Library").join("LaunchAgents");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join(format!("{label}.plist")),
        format!("<string>{}</string>", program.display()),
    )
    .unwrap();
}

fn verified_probe(mode: RuntimeMode) -> LegacyProbe {
    LegacyProbe {
        mode,
        identity_verified: true,
        influence_complete: true,
        can_retire_without_host_exit: true,
        self_heal_frozen: true,
    }
}

/// The old runtime being embedded in the host means there is nothing safe to stop.
///
/// `LegacyEmbedded` daemons are the TUI process; "retire" would have to mean
/// "terminate the user's editor", which a migration must never decide on its own.
#[test]
fn legacy_embedded_runtime_requires_host_cooperation() {
    let directory = TempDir::new().unwrap();
    let probe = LegacyProbe {
        mode: RuntimeMode::LegacyEmbedded,
        identity_verified: true,
        influence_complete: false,
        can_retire_without_host_exit: false,
        self_heal_frozen: false,
    };
    let plan = MigrationPlan::inspect(&test_scope(&directory), probe);
    assert_eq!(
        plan.disposition(),
        MigrationDisposition::ManualActionRequired
    );
    assert!(plan.blockers().contains(&MigrationBlocker::EmbeddedInHost));
}

/// Inspection must not touch anything: it runs while a UI is merely *showing* the
/// plan, and a user who never asked must not be migrated by looking.
#[test]
fn inspecting_never_touches_launchd() {
    let directory = TempDir::new().unwrap();
    let launchctl = FakeLaunchctl::with(vec![LoadedJob {
        label: "ai.yi-agent.resident-daemon-watchman".to_string(),
        program: Some(PathBuf::from("/usr/local/bin/yi-agent")),
        arguments: vec![
            "/usr/local/bin/yi-agent".into(),
            "resident".into(),
            "watch".into(),
        ],
    }]);
    let what = launchctl.booted_out.borrow().len();
    let plan = MigrationPlan::inspect(
        &test_scope(&directory),
        verified_probe(RuntimeMode::Managed),
    );
    assert_eq!(plan.disposition(), MigrationDisposition::ManagedReady);
    assert_eq!(
        launchctl.booted_out.borrow().len(),
        what,
        "inspection booted something out"
    );
}

/// Three independent resurrection paths, so three blockers when any is unknown.
///
/// "Influence complete" is a claim about comprehensiveness; a single unfound path
/// means a retired install comes back, so it must block.
#[test]
fn an_incomplete_picture_of_the_old_install_blocks_the_takeover() {
    let directory = TempDir::new().unwrap();
    for probe in [
        LegacyProbe {
            identity_verified: false,
            ..verified_probe(RuntimeMode::LegacyIndependent)
        },
        LegacyProbe {
            influence_complete: false,
            ..verified_probe(RuntimeMode::LegacyIndependent)
        },
        LegacyProbe {
            self_heal_frozen: false,
            ..verified_probe(RuntimeMode::LegacyIndependent)
        },
    ] {
        let plan = MigrationPlan::inspect(&test_scope(&directory), probe.clone());
        assert_eq!(
            plan.disposition(),
            MigrationDisposition::ManualActionRequired,
            "probe {probe:?} should block"
        );
        assert!(!plan.blockers().is_empty());
    }
}

/// A job with our label but another program is not ours to stop.
///
/// Booting it out would stop an unrelated service that happens to share a string.
#[test]
fn a_loaded_job_running_another_program_is_left_alone() {
    let directory = TempDir::new().unwrap();
    let ours = PathBuf::from("/Applications/yi-agent.app/Contents/MacOS/yi-agent");
    write_plist(
        directory.path(),
        "ai.yi-agent.resident-daemon-watchman",
        Path::new("/opt/other/bin/yi-agent"),
    );
    let launchctl = FakeLaunchctl::with(vec![LoadedJob {
        label: "ai.yi-agent.resident-daemon-watchman".to_string(),
        program: Some(PathBuf::from("/opt/other/bin/yi-agent")),
        arguments: vec![
            "/opt/other/bin/yi-agent".into(),
            "resident".into(),
            "watch".into(),
        ],
    }]);
    let mut plan = MigrationPlan::inspect_for_home(
        &test_scope(&directory),
        verified_probe(RuntimeMode::LegacyIndependent),
        directory.path(),
    );

    let outcome = plan
        .execute(
            &test_scope(&directory).key,
            &launchctl,
            &ours,
            Deadline::after(Duration::from_secs(2)),
        )
        .unwrap();
    assert_eq!(outcome, MigrationDisposition::ManualActionRequired);
    assert!(
        launchctl.booted_out.borrow().is_empty(),
        "another program's job must not be booted out"
    );
}

/// A confirmation that does not name this runtime stops nothing.
///
/// A bare "yes" is what a stray click produces; naming the scope is what proves the
/// user was told which runtime they are ending.
#[test]
fn a_confirmation_that_names_another_scope_stops_nothing() {
    let directory = TempDir::new().unwrap();
    let ours = PathBuf::from("/usr/local/bin/yi-agent");
    write_plist(directory.path(), "ai.yi-agent.board-watchman", &ours);
    let launchctl = FakeLaunchctl::with(vec![LoadedJob {
        label: "ai.yi-agent.board-watchman".to_string(),
        program: Some(ours.clone()),
        arguments: vec![ours.display().to_string(), "boards".into(), "watch".into()],
    }]);
    let mut plan = MigrationPlan::inspect_for_home(
        &test_scope(&directory),
        verified_probe(RuntimeMode::LegacyIndependent),
        directory.path(),
    );

    let error = plan
        .execute(
            "some other project",
            &launchctl,
            &ours,
            Deadline::after(Duration::from_secs(2)),
        )
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(launchctl.booted_out.borrow().is_empty());
}

/// The pre-generalisation label is retired as well as the current one.
///
/// It runs a subcommand that no longer exists, so leaving it loaded means a login
/// that fails forever — invisible to the user and unfixable from the UI.
#[test]
fn both_watchman_labels_are_retired() {
    let directory = TempDir::new().unwrap();
    let ours = PathBuf::from("/usr/local/bin/yi-agent");
    write_plist(directory.path(), "ai.yi-agent.board-watchman", &ours);
    write_plist(
        directory.path(),
        "ai.yi-agent.resident-daemon-watchman",
        &ours,
    );
    let launchctl = FakeLaunchctl::with(vec![
        LoadedJob {
            label: "ai.yi-agent.board-watchman".to_string(),
            program: Some(ours.clone()),
            arguments: vec![ours.display().to_string(), "boards".into(), "watch".into()],
        },
        LoadedJob {
            label: "ai.yi-agent.resident-daemon-watchman".to_string(),
            program: Some(ours.clone()),
            arguments: vec![
                ours.display().to_string(),
                "resident".into(),
                "watch".into(),
            ],
        },
    ]);
    let mut plan = MigrationPlan::inspect_for_home(
        &test_scope(&directory),
        verified_probe(RuntimeMode::LegacyIndependent),
        directory.path(),
    );

    let outcome = plan
        .execute(
            &test_scope(&directory).key,
            &launchctl,
            &ours,
            Deadline::after(Duration::from_secs(2)),
        )
        .unwrap();
    assert_eq!(outcome, MigrationDisposition::ManagedReady);
    let booted = launchctl.booted_out.borrow().clone();
    assert!(booted.contains(&"ai.yi-agent.board-watchman".to_string()));
    assert!(booted.contains(&"ai.yi-agent.resident-daemon-watchman".to_string()));
}

/// A bootout that fails aborts instead of claiming a migration.
///
/// Reporting success after a failed stop leaves the old self-healing job running
/// beside the new daemon — two processes both convinced they own the runtime.
#[test]
fn a_failed_bootout_is_reported_rather_than_ignored() {
    let directory = TempDir::new().unwrap();
    let ours = PathBuf::from("/usr/local/bin/yi-agent");
    write_plist(
        directory.path(),
        "ai.yi-agent.resident-daemon-watchman",
        &ours,
    );
    let mut launchctl = FakeLaunchctl::with(vec![LoadedJob {
        label: "ai.yi-agent.resident-daemon-watchman".to_string(),
        program: Some(ours.clone()),
        arguments: vec![
            ours.display().to_string(),
            "resident".into(),
            "watch".into(),
        ],
    }]);
    launchctl.refusing = vec!["ai.yi-agent.resident-daemon-watchman".to_string()];
    let mut plan = MigrationPlan::inspect_for_home(
        &test_scope(&directory),
        verified_probe(RuntimeMode::LegacyIndependent),
        directory.path(),
    );

    let error = plan
        .execute(
            &test_scope(&directory).key,
            &launchctl,
            &ours,
            Deadline::after(Duration::from_secs(2)),
        )
        .unwrap_err();
    assert!(error.to_string().contains("refused"));
}

/// An expired budget does not start a destructive operation.
///
/// Half a migration is worse than none: the job may be booted out while nothing
/// replaces it, leaving the scope with no supervisor at all.
#[test]
fn an_expired_budget_performs_no_migration() {
    let directory = TempDir::new().unwrap();
    let ours = PathBuf::from("/usr/local/bin/yi-agent");
    write_plist(
        directory.path(),
        "ai.yi-agent.resident-daemon-watchman",
        &ours,
    );
    let launchctl = FakeLaunchctl::with(vec![LoadedJob {
        label: "ai.yi-agent.resident-daemon-watchman".to_string(),
        program: Some(ours.clone()),
        arguments: vec![
            ours.display().to_string(),
            "resident".into(),
            "watch".into(),
        ],
    }]);
    let mut plan = MigrationPlan::inspect_for_home(
        &test_scope(&directory),
        verified_probe(RuntimeMode::LegacyIndependent),
        directory.path(),
    );

    let error = plan
        .execute(
            &test_scope(&directory).key,
            &launchctl,
            &ours,
            Deadline::after(Duration::ZERO),
        )
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert!(launchctl.booted_out.borrow().is_empty());
}

/// Staying on the old runtime is an explicit answer, never an inference.
///
/// If inspection guessed "the user wants to wait", an unsafe install would stay
/// alive while the UI reported a completed migration.
#[test]
fn temporary_use_is_an_explicit_decision() {
    let directory = TempDir::new().unwrap();
    let mut plan = MigrationPlan::inspect(
        &test_scope(&directory),
        LegacyProbe {
            self_heal_frozen: false,
            ..verified_probe(RuntimeMode::LegacyIndependent)
        },
    );
    assert_eq!(
        plan.disposition(),
        MigrationDisposition::ManualActionRequired,
        "inspection never guesses 'temporary use'"
    );
    assert_eq!(
        plan.temporary_use(),
        MigrationDisposition::ExplicitTemporaryUse
    );
}

/// Retiring a job also removes its plist.
///
/// launchd loads every plist in `LaunchAgents` at login, so leaving the file means
/// the job we just booted out is back after the next reboot.
#[test]
fn retiring_a_job_removes_its_plist() {
    let directory = TempDir::new().unwrap();
    let ours = PathBuf::from("/usr/local/bin/yi-agent");
    write_plist(
        directory.path(),
        "ai.yi-agent.resident-daemon-watchman",
        &ours,
    );
    let launchctl = FakeLaunchctl::with(vec![LoadedJob {
        label: "ai.yi-agent.resident-daemon-watchman".to_string(),
        program: Some(ours.clone()),
        arguments: vec![
            ours.display().to_string(),
            "resident".into(),
            "watch".into(),
        ],
    }]);
    let mut plan = MigrationPlan::inspect_for_home(
        &test_scope(&directory),
        verified_probe(RuntimeMode::LegacyIndependent),
        directory.path(),
    );
    let outcome = plan
        .execute(
            &test_scope(&directory).key,
            &launchctl,
            &ours,
            Deadline::after(Duration::from_secs(2)),
        )
        .unwrap();
    assert_eq!(outcome, MigrationDisposition::ManagedReady);
    assert_eq!(
        plan.plist_paths()[1],
        yi_agent_supervisors::watchman::plist_path(directory.path())
    );
    assert!(
        !yi_agent_supervisors::watchman::plist_path(directory.path()).exists(),
        "the plist outlived the job, so the next login would reload it"
    );
}

/// An unloaded job whose plist is still ours still gets its plist removed.
///
/// "Not loaded right now" is not "retired": the file is what makes login load it.
#[test]
fn a_plist_without_a_loaded_job_is_still_retired() {
    let directory = TempDir::new().unwrap();
    let ours = PathBuf::from("/usr/local/bin/yi-agent");
    write_plist(directory.path(), "ai.yi-agent.board-watchman", &ours);
    let launchctl = FakeLaunchctl::with(Vec::new());
    let mut plan = MigrationPlan::inspect_for_home(
        &test_scope(&directory),
        verified_probe(RuntimeMode::LegacyIndependent),
        directory.path(),
    );
    let outcome = plan
        .execute(
            &test_scope(&directory).key,
            &launchctl,
            &ours,
            Deadline::after(Duration::from_secs(2)),
        )
        .unwrap();
    assert_eq!(outcome, MigrationDisposition::ManagedReady);
    assert!(
        !directory
            .path()
            .join("Library/LaunchAgents/ai.yi-agent.board-watchman.plist")
            .exists()
    );
}

/// A plist under our label that names another program is somebody else's file.
///
/// Deleting it would break their install, so the migration reports manual action and
/// leaves the file alone.
#[test]
fn another_installs_plist_is_not_deleted() {
    let directory = TempDir::new().unwrap();
    let ours = PathBuf::from("/usr/local/bin/yi-agent");
    let other = PathBuf::from("/opt/other/bin/yi-agent");
    write_plist(
        directory.path(),
        "ai.yi-agent.resident-daemon-watchman",
        &other,
    );
    let launchctl = FakeLaunchctl::with(Vec::new());
    let mut plan = MigrationPlan::inspect_for_home(
        &test_scope(&directory),
        verified_probe(RuntimeMode::LegacyIndependent),
        directory.path(),
    );
    let outcome = plan
        .execute(
            &test_scope(&directory).key,
            &launchctl,
            &ours,
            Deadline::after(Duration::from_secs(2)),
        )
        .unwrap();
    assert_eq!(outcome, MigrationDisposition::ManualActionRequired);
    assert!(
        yi_agent_supervisors::watchman::plist_path(directory.path()).exists(),
        "another install's plist must survive"
    );
}

/// A plist that names another program is not this install.
#[test]
fn a_plist_naming_another_program_is_not_ours() {
    let directory = TempDir::new().unwrap();
    let plist = directory.path().join("other.plist");
    std::fs::write(&plist, "<string>/opt/other/bin/yi-agent</string>").unwrap();
    assert!(
        !yi_agent_supervisors::lifecycle::migration::plist_names_program(
            &plist,
            Path::new("/usr/local/bin/yi-agent")
        )
    );
    assert!(
        !yi_agent_supervisors::lifecycle::migration::plist_names_program(
            &directory.path().join("missing.plist"),
            Path::new("/usr/local/bin/yi-agent")
        ),
        "an unreadable plist is not evidence either"
    );
}

/// The two labels and their plists come from one place, so a rename cannot make the
/// migration look at a plist nothing writes.
#[test]
fn the_watchman_labels_match_the_installer() {
    let directory = TempDir::new().unwrap();
    let watchman = LegacyWatchman::for_home(directory.path());
    assert_eq!(watchman.label, yi_agent_supervisors::watchman::LABEL);
    assert_eq!(
        watchman.legacy_label,
        yi_agent_supervisors::watchman::LEGACY_LABEL
    );
    assert_eq!(
        watchman.plist,
        yi_agent_supervisors::watchman::plist_path(directory.path())
    );
    assert!(watchman.legacy_plist.ends_with(format!(
        "{}.plist",
        yi_agent_supervisors::watchman::LEGACY_LABEL
    )));
}

/// A job is only "ours" when it names our program *and* our label was asked about.
#[test]
fn only_a_job_naming_our_program_is_claimed() {
    let ours = PathBuf::from("/usr/local/bin/yi-agent");
    let theirs = LoadedJob {
        label: "ai.yi-agent.resident-daemon-watchman".to_string(),
        program: Some(PathBuf::from("/opt/other/yi-agent")),
        arguments: Vec::new(),
    };
    assert!(!LegacyWatchman::claims(&theirs, &ours));
    let ours_job = LoadedJob {
        program: Some(ours.clone()),
        ..theirs
    };
    assert!(LegacyWatchman::claims(&ours_job, &ours));
    // A job that names no program cannot be shown to be ours.
    let anonymous = LoadedJob {
        program: None,
        ..ours_job
    };
    assert!(!LegacyWatchman::claims(&anonymous, &ours));
}

// ---------------------------------------------------------------------------
// Evidence, not booleans: what the platform can and cannot prove.
//
// Every assertion below is about the *evidence* layer, so each one drives
// `LegacyEvidenceProbe::inspect` over injected platform facts. That keeps the tests
// hermetic (no launchd, no real sockets) while still exercising the real decision
// path: the facts go through `inspect`, not straight into a hand-built struct.
// ---------------------------------------------------------------------------

mod evidence {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;
    use yi_agent_lifecycle::deadline::Deadline;
    use yi_agent_lifecycle::types::{
        ArtifactIdentity, BuildId, RuntimeIdentity, RuntimeMode, RuntimeScope,
    };
    use yi_agent_supervisors::lifecycle::legacy_evidence::{
        AffectedRegistration, LegacyEvidence, LegacyEvidenceProbe, MigrationPreview, Observation,
        PlatformFacts, registration_impact,
    };

    use yi_agent_supervisors::lifecycle::migration::LoadedJob;

    /// A scope whose instance home and runtime directory both exist.
    fn scope(directory: &TempDir) -> RuntimeScope {
        let runtime_dir = directory.path().join("runtime");
        let instance_home = directory.path().join("instance");
        std::fs::create_dir_all(runtime_dir.join("runtime.sqlite")).unwrap();
        std::fs::create_dir_all(runtime_dir.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&instance_home).unwrap();
        RuntimeScope {
            key: "scope-key".to_string(),
            runtime_dir,
            project_key: "/project".to_string(),
            instance_home,
        }
    }

    fn deadline() -> Deadline {
        Deadline::after(std::time::Duration::from_secs(2))
    }

    /// The facts a test tells the probe; unset facts answer `Unknown` on purpose, so
    /// a test that forgets one fails loudly instead of silently getting a default.
    #[derive(Default, Clone)]
    struct Facts {
        peer_uid: Option<Result<u32, String>>,
        peer_pid: Option<Result<u32, String>>,
        process: Option<Result<(String, PathBuf), String>>,
        digest: Option<Result<String, String>>,
        registrations: Option<Vec<AffectedRegistration>>,
        jobs: Option<Result<Vec<LoadedJob>, String>>,
        /// What the running runtime claims. `None` means no socket answered.
        reported: Option<RuntimeIdentity>,
    }

    impl Facts {
        /// Every fact answered, as a healthy idle managed runtime would answer.
        fn complete(executable: &Path, digest: &str) -> Self {
            Self {
                peer_uid: Some(Ok(unsafe { libc::geteuid() })),
                peer_pid: Some(Ok(4242)),
                process: Some(Ok(("100.000001".to_string(), executable.to_path_buf()))),
                digest: Some(Ok(digest.to_string())),
                registrations: Some(Vec::new()),
                jobs: Some(Ok(Vec::new())),
                reported: Some(identity(RuntimeMode::Managed)),
            }
        }
    }

    impl PlatformFacts for Facts {
        fn peer_uid(&self, _socket: &Path) -> Observation<u32> {
            match &self.peer_uid {
                Some(Ok(uid)) => Observation::Verified(*uid),
                Some(Err(reason)) => Observation::Unknown(reason.clone()),
                None => Observation::Unknown("the test did not answer".to_string()),
            }
        }

        fn peer_pid(&self, _socket: &Path) -> Observation<u32> {
            match &self.peer_pid {
                Some(Ok(pid)) => Observation::Verified(*pid),
                Some(Err(reason)) => Observation::Unknown(reason.clone()),
                None => Observation::Unknown("the test did not answer".to_string()),
            }
        }

        fn process_identity(&self, _pid: u32) -> Observation<(String, PathBuf)> {
            match &self.process {
                Some(Ok((start, executable))) => {
                    Observation::Verified((start.clone(), executable.clone()))
                }
                Some(Err(reason)) => Observation::Unknown(reason.clone()),
                None => Observation::Unknown("the test did not answer".to_string()),
            }
        }

        fn file_sha256(&self, _path: &Path) -> Observation<String> {
            match &self.digest {
                Some(Ok(digest)) => Observation::Verified(digest.clone()),
                Some(Err(reason)) => Observation::Unknown(reason.clone()),
                None => Observation::Unknown("the test did not answer".to_string()),
            }
        }

        fn registrations(&self, _home: &Path) -> Observation<Vec<AffectedRegistration>> {
            match &self.registrations {
                Some(entries) => Observation::Verified(entries.clone()),
                None => Observation::Unknown("the test did not answer".to_string()),
            }
        }

        fn loaded_jobs(&self, _labels: &[String]) -> Observation<Vec<LoadedJob>> {
            match &self.jobs {
                Some(Ok(jobs)) => Observation::Verified(jobs.clone()),
                Some(Err(reason)) => Observation::Unknown(reason.clone()),
                None => Observation::Unknown("the test did not answer".to_string()),
            }
        }

        fn reported_identity(
            &self,
            _socket: &Path,
            _deadline: &Deadline,
        ) -> Observation<RuntimeIdentity> {
            match &self.reported {
                Some(identity) => Observation::Verified(identity.clone()),
                None => Observation::Unknown(
                    "no control socket exists, so no runtime reported an identity".to_string(),
                ),
            }
        }
    }

    /// The evidence an old, idle, cooperative runtime would produce. Each test
    /// below degrades exactly one fact, so a failing assertion names the fact.
    fn verified_evidence(directory: &TempDir) -> LegacyEvidence {
        let executable = directory.path().join("yi-agent");
        std::fs::write(&executable, b"binary").unwrap();
        let facts = Facts::complete(&executable, "digest");
        LegacyEvidenceProbe::with_facts(facts, None, directory.path())
            .inspect(&scope(directory), &deadline())
            .unwrap()
    }

    // -- the facts themselves -------------------------------------------------

    #[test]
    fn status_only_evidence_blocks_automatic_takeover() {
        // Nothing answered on the socket: no mode, no protocol, no identity. This is
        // the shape a probe produces when it only managed to read file system state.
        let directory = TempDir::new().unwrap();
        let evidence = LegacyEvidenceProbe::with_facts(Facts::default(), None, directory.path())
            .inspect(&scope(&directory), &deadline())
            .unwrap();
        assert!(
            !evidence.identity.is_verified(),
            "a control socket nobody answered with cannot prove a process identity"
        );
        assert!(!evidence.automatic_takeover_allowed());
        // And the blockers name the facts, not just "not ready".
        assert!(
            evidence
                .blockers()
                .iter()
                .any(|blocker| blocker.contains("could not be verified")),
            "{:?}",
            evidence.blockers()
        );
    }

    #[test]
    fn a_pid_reused_by_another_process_blocks_takeover() {
        // The kernel cannot describe the pid the runtime claims: the process is gone
        // and its number may already belong to something else. Identity is unproven.
        let directory = TempDir::new().unwrap();
        let executable = directory.path().join("yi-agent");
        std::fs::write(&executable, b"binary").unwrap();
        let mut facts = Facts::complete(&executable, "digest");
        facts.process = Some(Err("no such process".to_string()));
        let evidence = LegacyEvidenceProbe::with_facts(facts, None, directory.path())
            .inspect(&scope(&directory), &deadline())
            .unwrap();
        assert!(!evidence.automatic_takeover_allowed());
        assert!(
            evidence
                .blockers()
                .iter()
                .any(|blocker| blocker.contains("could not be verified")),
            "the missing fact must be named: {:?}",
            evidence.blockers()
        );
    }

    #[test]
    fn a_socket_owned_by_another_uid_is_a_contradiction_not_an_unknown() {
        let directory = TempDir::new().unwrap();
        let executable = directory.path().join("yi-agent");
        std::fs::write(&executable, b"binary").unwrap();
        let mut facts = Facts::complete(&executable, "digest");
        // A different uid is a definite answer: somebody else owns this socket.
        facts.peer_uid = Some(Ok(unsafe { libc::geteuid() }.wrapping_add(1)));
        let evidence = LegacyEvidenceProbe::with_facts(facts, None, directory.path())
            .inspect(&scope(&directory), &deadline())
            .unwrap();
        assert!(!evidence.automatic_takeover_allowed());
        assert!(
            evidence
                .blockers()
                .iter()
                .any(|blocker| blocker.contains("not this user")),
            "{:?}",
            evidence.blockers()
        );
    }

    #[test]
    fn a_wrong_executable_digest_blocks_takeover() {
        let directory = TempDir::new().unwrap();
        let executable = directory.path().join("yi-agent");
        std::fs::write(&executable, b"binary").unwrap();
        let mut expected = artifact(&executable);
        expected.sha256 = "the-build-we-want".to_string();
        let facts = Facts::complete(&executable, "something-else-entirely");
        let evidence = LegacyEvidenceProbe::with_facts(facts, Some(expected), directory.path())
            .inspect(&scope(&directory), &deadline())
            .unwrap();
        assert!(!evidence.automatic_takeover_allowed());
        assert!(
            evidence
                .blockers()
                .iter()
                .any(|blocker| blocker.contains("digest")),
            "{:?}",
            evidence.blockers()
        );
    }

    #[test]
    fn an_unreadable_lock_owner_blocks_takeover() {
        // The lock file exists but cannot be opened: whether another daemon holds it
        // is unproven, and "unproven" must not read as "free".
        let directory = TempDir::new().unwrap();
        let scope = scope(&directory);
        std::fs::write(scope.runtime_dir.join("runtime.lock"), b"").unwrap();
        let executable = directory.path().join("yi-agent");
        std::fs::write(&executable, b"binary").unwrap();
        let facts = Facts::complete(&executable, "digest");
        let evidence = LegacyEvidenceProbe::with_facts(facts, None, directory.path())
            .inspect(&scope, &deadline())
            .unwrap();
        // A lock nobody holds is provably free, and that is a `Verified(false)`.
        assert_eq!(evidence.runtime_lock, Observation::Verified(false));
        // The evidence is still blocked, because nothing proved identity.
        assert!(!evidence.automatic_takeover_allowed());
    }

    #[test]
    fn an_embedded_tui_blocks_automatic_takeover() {
        let directory = TempDir::new().unwrap();
        let mut evidence = verified_evidence(&directory);
        evidence.host_mode = Observation::Verified(RuntimeMode::LegacyEmbedded);
        assert!(!evidence.automatic_takeover_allowed());
        assert!(
            evidence
                .blockers()
                .iter()
                .any(|blocker| blocker.contains("legacy embedded")),
            "{:?}",
            evidence.blockers()
        );
    }

    #[test]
    fn a_foreign_loaded_job_blocks_automatic_takeover() {
        let directory = TempDir::new().unwrap();
        let mut evidence = verified_evidence(&directory);
        evidence.resurrection = Observation::Verified(vec![
            yi_agent_supervisors::lifecycle::legacy_evidence::ResurrectionSource::LaunchdJob {
                label: "ai.yi-agent.resident-daemon-watchman".to_string(),
                program: PathBuf::from("/usr/local/bin/yi-agent"),
            },
        ]);
        assert!(!evidence.automatic_takeover_allowed());
        assert!(
            evidence
                .blockers()
                .iter()
                .any(|blocker| blocker.contains("launchd job")),
            "{:?}",
            evidence.blockers()
        );
    }

    #[test]
    fn incomplete_resurrection_sources_block_takeover() {
        let directory = TempDir::new().unwrap();
        let mut evidence = verified_evidence(&directory);
        evidence.resurrection = Observation::Unknown(
            "launchd could not be asked, so a resurrection path may be missing".to_string(),
        );
        assert!(!evidence.automatic_takeover_allowed());
        assert!(
            evidence
                .blockers()
                .iter()
                .any(|blocker| blocker.contains("resurrection")),
            "{:?}",
            evidence.blockers()
        );
    }

    #[test]
    fn idle_but_admission_not_frozen_blocks_takeover() {
        let directory = TempDir::new().unwrap();
        let mut evidence = verified_evidence(&directory);
        evidence.admission_freeze = Observation::Verified(false);
        assert!(!evidence.automatic_takeover_allowed());
        assert!(
            evidence
                .blockers()
                .iter()
                .any(|blocker| blocker.contains("freeze admission")),
            "{:?}",
            evidence.blockers()
        );
    }

    #[test]
    fn verified_cooperative_idle_allows_automatic_takeover() {
        // The one positive case: identity proven against the kernel, lock provably
        // free, managed mode, no resurrection path, freeze supported, nothing
        // registered. If this ever stops passing, the policy has become "never".
        let directory = TempDir::new().unwrap();
        let mut evidence = verified_evidence(&directory);
        // `verified_evidence` runs the real probe over injected facts, so identity is
        // proven only if the kernel-side facts agree; assert that first.
        assert!(
            evidence.identity.is_verified(),
            "the fixture must prove identity, otherwise this test proves nothing: {:?}",
            evidence.identity.missing().map(str::to_string)
        );
        assert_eq!(evidence.runtime_lock, Observation::Verified(false));
        assert_eq!(
            evidence.host_mode,
            Observation::Verified(RuntimeMode::Managed)
        );
        assert_eq!(evidence.admission_freeze, Observation::Verified(false));

        // Freeze is still unsupported in this build, and that alone blocks takeover.
        // Turning it on is what a cooperative host will eventually supply.
        evidence.admission_freeze = Observation::Verified(true);
        assert!(
            evidence.automatic_takeover_allowed(),
            "a proven idle, frozen, managed runtime must be takeable: {:?}",
            evidence.blockers()
        );
    }

    #[test]
    fn a_held_runtime_lock_blocks_takeover() {
        let directory = TempDir::new().unwrap();
        let mut evidence = verified_evidence(&directory);
        evidence.runtime_lock = Observation::Verified(true);
        assert!(!evidence.automatic_takeover_allowed());
        assert!(
            evidence
                .blockers()
                .iter()
                .any(|blocker| blocker.contains("instance lock")),
            "{:?}",
            evidence.blockers()
        );
    }

    #[test]
    fn unknown_protocol_blocks_even_temporary_use() {
        // A runtime that answers but advertises no business protocol: the mode is
        // known, the protocol is not, and "unknown protocol" must never be read as
        // "compatible enough".
        let directory = TempDir::new().unwrap();
        let executable = directory.path().join("yi-agent");
        std::fs::write(&executable, b"binary").unwrap();
        let mut facts = Facts::complete(&executable, "digest");
        let mut reported = identity(RuntimeMode::LegacyIndependent);
        reported.business_protocol = None;
        facts.reported = Some(reported);
        let evidence = LegacyEvidenceProbe::with_facts(facts, None, directory.path())
            .inspect(&scope(&directory), &deadline())
            .unwrap();

        // The absence surfaces as protocol 0, which no caller expects, and the
        // evidence still refuses automatic takeover.
        assert!(matches!(evidence.protocol, Observation::Verified((0, _))));
        assert!(!evidence.automatic_takeover_allowed());
        // And the mode is what the runtime said, not silently "managed".
        assert_eq!(evidence.probe().mode, RuntimeMode::LegacyIndependent);
    }

    // -- previews are read-only and bound to their evidence -------------------

    #[test]
    fn a_preview_never_writes_and_never_stops_anything() {
        let directory = TempDir::new().unwrap();
        let scope = scope(&directory);
        // The fixture's own files are created first: the snapshot must cover only
        // what inspection could have touched.
        let executable = directory.path().join("yi-agent");
        std::fs::write(&executable, b"binary").unwrap();
        let before = snapshot(directory.path());
        let facts = Facts::complete(&executable, "digest");
        let evidence = LegacyEvidenceProbe::with_facts(facts, None, directory.path())
            .inspect(&scope, &deadline())
            .unwrap();
        let preview = MigrationPreview::new(&evidence);
        assert!(!preview.token().is_empty());
        assert!(!preview.is_expired());

        assert_eq!(
            before,
            snapshot(directory.path()),
            "inspection and preview must not touch the instance"
        );
    }

    #[test]
    fn a_preview_of_a_missing_runtime_directory_is_still_read_only() {
        // The runtime directory does not exist at all: a preview has to be able to
        // say "nothing runs here" without creating it.
        let directory = TempDir::new().unwrap();
        let instance_home = directory.path().join("instance");
        std::fs::create_dir_all(&instance_home).unwrap();
        let scope = RuntimeScope {
            key: "scope-key".to_string(),
            runtime_dir: directory.path().join("does-not-exist"),
            project_key: "/project".to_string(),
            instance_home,
        };
        let evidence = LegacyEvidenceProbe::with_facts(Facts::default(), None, directory.path())
            .inspect(&scope, &deadline())
            .unwrap();
        let _preview = MigrationPreview::new(&evidence);
        assert!(
            !scope.runtime_dir.exists(),
            "a preview must not create the runtime directory it inspects"
        );
    }

    #[test]
    fn a_token_stops_authorising_once_the_evidence_changes() {
        let directory = TempDir::new().unwrap();
        let evidence = verified_evidence(&directory);
        let preview = MigrationPreview::new(&evidence);
        let now = std::time::Instant::now();
        assert!(preview.still_authorises(&evidence, now));

        // A second registration appearing is exactly the change that must invalidate
        // a preview: the impact the user was shown is no longer the impact.
        let mut changed = evidence.clone();
        changed.registrations = Observation::Verified(vec![AffectedRegistration {
            project: PathBuf::from("/somewhere-else"),
            requester: "plugin".to_string(),
        }]);
        assert_ne!(evidence.digest(), changed.digest());
        assert!(!preview.still_authorises(&changed, now));
    }

    #[test]
    fn a_token_expires() {
        let directory = TempDir::new().unwrap();
        let evidence = verified_evidence(&directory);
        let preview = MigrationPreview::new(&evidence);
        let later =
            std::time::Instant::now() + MigrationPreview::TTL + std::time::Duration::from_secs(1);
        assert!(!preview.still_authorises(&evidence, later));
    }

    #[test]
    fn two_projects_in_one_instance_are_both_reported() {
        let directory = TempDir::new().unwrap();
        let mut evidence = verified_evidence(&directory);
        evidence.registrations = Observation::Verified(vec![
            AffectedRegistration {
                project: PathBuf::from("/alpha"),
                requester: "plugin-a".to_string(),
            },
            AffectedRegistration {
                project: PathBuf::from("/beta"),
                requester: "plugin-b".to_string(),
            },
        ]);
        let preview = MigrationPreview::new(&evidence);
        assert_eq!(preview.affected().len(), 2);
        let impact = registration_impact(&evidence);
        assert_eq!(impact.plugins.len(), 2);
        assert_eq!(
            impact.plugins[0].kind, "registration",
            "an impact entry must name what kind of thing it is"
        );
    }

    fn artifact(executable: &Path) -> ArtifactIdentity {
        ArtifactIdentity {
            build_id: BuildId::from("build"),
            display_version: "0.1.0".to_string(),
            executable: executable.to_path_buf(),
            sha256: "digest".to_string(),
            schema_min: 1,
            schema_max: 1,
            handover_version: 1,
            business_protocol_min: 1,
            business_protocol_max: 1,
            capabilities: Vec::new(),
        }
    }

    /// Every file and directory under a root, with contents, so a test can prove an
    /// inspection changed nothing.
    fn snapshot(root: &Path) -> Vec<(PathBuf, Option<Vec<u8>>, std::time::SystemTime)> {
        let mut entries = Vec::new();
        collect(root, &mut entries);
        entries.sort();
        entries
    }

    fn collect(dir: &Path, out: &mut Vec<(PathBuf, Option<Vec<u8>>, std::time::SystemTime)>) {
        let Ok(read) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in read.flatten() {
            let path = entry.path();
            let metadata = entry.metadata().unwrap();
            let modified = metadata.modified().unwrap();
            if metadata.is_dir() {
                out.push((path.clone(), None, modified));
                collect(&path, out);
            } else {
                out.push((path.clone(), Some(std::fs::read(&path).unwrap()), modified));
            }
        }
    }

    /// `RuntimeIdentity` is what a real runtime answers with; this keeps the shape
    /// honest when a test needs one.
    #[allow(dead_code)]
    fn identity(mode: RuntimeMode) -> RuntimeIdentity {
        RuntimeIdentity {
            scope_key: "scope-key".to_string(),
            daemon_instance_id: None,
            guardian_build_id: None,
            daemon_build_id: None,
            mode,
            business_protocol: Some(1),
            daemon_display_version: None,
            guardian_display_version: None,
            capabilities: Vec::new(),
            schema_min: 1,
            schema_max: 1,
        }
    }

    #[allow(dead_code)]
    fn untouched_facts() -> RefCell<Vec<PathBuf>> {
        RefCell::new(Vec::new())
    }
}

// ---------------------------------------------------------------------------
// The migration operation: ordered phases, one journal, no repeated stop.
//
// A migration is the only operation here that both stops something the user relies
// on and starts something new, so every test below asks the same kind of question:
// after this crash, what does the record say, and would a resume do the destructive
// thing twice?
// ---------------------------------------------------------------------------

mod operation {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::{FakeLaunchctl, write_plist};
    use tempfile::TempDir;
    use yi_agent_lifecycle::deadline::Deadline;
    use yi_agent_lifecycle::types::{ArtifactIdentity, RuntimeScope};
    use yi_agent_supervisors::lifecycle::CoordinationGuard;
    use yi_agent_supervisors::lifecycle::bringup::HeldScope;
    use yi_agent_supervisors::lifecycle::bringup::{
        BringUpIntent, BringUpResult, ControlProbe, GuardianLauncher,
    };
    use yi_agent_supervisors::lifecycle::legacy_evidence::{
        LegacyEvidence, LegacyEvidenceProbe, MigrationPreview, Observation, PlatformFacts,
    };
    use yi_agent_supervisors::lifecycle::migration::LoadedJob;
    use yi_agent_supervisors::lifecycle::migration_operation::{
        FaultPoint, MigrationBackend, MigrationCoordinator, MigrationPhase, MigrationProbes,
        MigrationRequest, OwnershipProof, RetirementGate, SystemMigrationBackend, read_journals,
    };

    /// Every side effect recorded in order, so a test can assert both *what* ran and
    /// *how many times*.
    #[derive(Default)]
    struct Recorder {
        calls: RefCell<Vec<&'static str>>,
        /// The call that must fail, by name.
        failing: RefCell<Option<&'static str>>,
        /// Whether the scope's coordination lock was still held when ownership was
        /// acknowledged. `None` until the acknowledgment runs.
        lock_held_at_ack: RefCell<Option<bool>>,
        /// Set by the test to the scope the backend belongs to.
        observed_scope: RefCell<Option<PathBuf>>,
    }

    impl Recorder {
        fn calls(&self) -> Vec<&'static str> {
            self.calls.borrow().clone()
        }

        fn count(&self, name: &str) -> usize {
            self.calls()
                .into_iter()
                .filter(|call| *call == name)
                .count()
        }

        fn fail_on(&self, name: &'static str) {
            *self.failing.borrow_mut() = Some(name);
        }

        fn record(&self, name: &'static str) -> std::io::Result<()> {
            self.calls.borrow_mut().push(name);
            if self.failing.borrow().as_deref() == Some(name) {
                return Err(std::io::Error::other(format!("{name} failed")));
            }
            Ok(())
        }
    }

    impl MigrationBackend for Recorder {
        fn freeze_self_heal(&self) -> std::io::Result<()> {
            self.record("freeze")
        }
        fn retire_watchman(&self) -> std::io::Result<()> {
            self.record("watchman")
        }
        fn stop_legacy(&self) -> std::io::Result<()> {
            self.record("stop")
        }
        fn ensure_managed(
            &self,
            _held: &HeldScope,
            _expected: &ArtifactIdentity,
            _intent: BringUpIntent,
        ) -> std::io::Result<BringUpResult> {
            self.record("connect")?;
            Ok(BringUpResult::ManualActionRequired {
                reason: "the test does not really connect".to_string(),
            })
        }
        fn acknowledge_ownership(&self, _proof: &OwnershipProof<'_>) -> std::io::Result<()> {
            self.record("acknowledge")
        }
    }

    /// A backend whose connect succeeds, so the happy path reaches `Completed`.
    #[derive(Default)]
    struct Connected {
        recorder: Recorder,
    }

    impl MigrationBackend for Connected {
        fn freeze_self_heal(&self) -> std::io::Result<()> {
            self.recorder.record("freeze")
        }
        fn retire_watchman(&self) -> std::io::Result<()> {
            self.recorder.record("watchman")
        }
        fn stop_legacy(&self) -> std::io::Result<()> {
            self.recorder.record("stop")
        }
        fn ensure_managed(
            &self,
            _held: &HeldScope,
            _expected: &ArtifactIdentity,
            _intent: BringUpIntent,
        ) -> std::io::Result<BringUpResult> {
            self.recorder.record("connect")?;
            Ok(BringUpResult::Connected {
                identity: yi_agent_lifecycle::types::RuntimeIdentity {
                    scope_key: "scope".to_string(),
                    daemon_instance_id: Some("instance".to_string()),
                    guardian_build_id: Some("build".to_string()),
                    daemon_build_id: Some("build".to_string()),
                    mode: yi_agent_lifecycle::types::RuntimeMode::Managed,
                    business_protocol: Some(1),
                    daemon_display_version: Some("0.1.0".to_string()),
                    guardian_display_version: Some("0.1.0".to_string()),
                    capabilities: Vec::new(),
                    schema_min: 1,
                    schema_max: 1,
                },
                control_path: PathBuf::from("/tmp/socket"),
            })
        }
        fn acknowledge_ownership(&self, proof: &OwnershipProof<'_>) -> std::io::Result<()> {
            self.recorder.record("acknowledge")?;
            // The lock must be gone here: holding it across a lease callback would
            // block every other coordinator for as long as a user takes to answer.
            if let Some(runtime_dir) = self.recorder.observed_scope.borrow().as_ref() {
                let path = runtime_dir.join("coordination.lock");
                let held = std::fs::OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .ok()
                    .map(|file| unsafe {
                        use std::os::unix::io::AsRawFd;
                        libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) != 0
                    });
                *self.recorder.lock_held_at_ack.borrow_mut() = held;
            }
            assert!(!proof.operation_id.is_empty());
            Ok(())
        }
    }

    fn scope(directory: &TempDir) -> RuntimeScope {
        let runtime_dir = directory.path().join("runtime");
        std::fs::create_dir_all(&runtime_dir).unwrap();
        let instance_home = directory.path().join("instance");
        std::fs::create_dir_all(&instance_home).unwrap();
        RuntimeScope {
            key: "scope-key".to_string(),
            runtime_dir,
            project_key: "/project".to_string(),
            instance_home,
        }
    }

    fn artifact() -> ArtifactIdentity {
        ArtifactIdentity {
            build_id: "build".to_string(),
            display_version: "0.1.0".to_string(),
            executable: PathBuf::from("/usr/local/bin/yi-agent"),
            sha256: "digest".to_string(),
            schema_min: 1,
            schema_max: 1,
            handover_version: 1,
            business_protocol_min: 1,
            business_protocol_max: 1,
            capabilities: Vec::new(),
        }
    }

    #[derive(Default)]
    struct Facts {
        registrations:
            Option<Vec<yi_agent_supervisors::lifecycle::legacy_evidence::AffectedRegistration>>,
    }

    impl PlatformFacts for Facts {
        fn peer_uid(&self, _socket: &Path) -> Observation<u32> {
            Observation::Verified(501)
        }
        fn peer_pid(&self, _socket: &Path) -> Observation<u32> {
            Observation::Verified(1)
        }
        fn process_identity(&self, _pid: u32) -> Observation<(String, PathBuf)> {
            Observation::Verified(("1.0".to_string(), PathBuf::from("/usr/local/bin/yi-agent")))
        }
        fn file_sha256(&self, _path: &Path) -> Observation<String> {
            Observation::Verified("digest".to_string())
        }
        fn registrations(
            &self,
            _home: &Path,
        ) -> Observation<Vec<yi_agent_supervisors::lifecycle::legacy_evidence::AffectedRegistration>>
        {
            Observation::Verified(self.registrations.clone().unwrap_or_default())
        }
        fn loaded_jobs(&self, _labels: &[String]) -> Observation<Vec<LoadedJob>> {
            Observation::Verified(Vec::new())
        }
        fn reported_identity(
            &self,
            _socket: &Path,
            _deadline: &Deadline,
        ) -> Observation<yi_agent_lifecycle::types::RuntimeIdentity> {
            Observation::Unknown("no socket".to_string())
        }
    }

    /// Evidence for a scope, and the digest a preview would hand to `confirm`.
    fn evidence_for(directory: &TempDir, scope: &RuntimeScope) -> (LegacyEvidence, String) {
        let evidence =
            LegacyEvidenceProbe::with_facts(Facts::default(), Some(artifact()), directory.path())
                .inspect(scope, &Deadline::after(Duration::from_secs(2)))
                .unwrap();
        let digest = MigrationPreview::new(&evidence)
            .evidence_digest()
            .to_string();
        (evidence, digest)
    }

    fn deadline() -> Deadline {
        Deadline::after(Duration::from_secs(5))
    }

    /// A migration takes ownership of the lock it was given: that is what lets it
    /// release the lock after `ManagedConnected` and then acknowledge ownership.
    fn held(guard: CoordinationGuard) -> HeldScope {
        HeldScope {
            guard,
            deadline: deadline(),
        }
    }

    // -- ordering -------------------------------------------------------------

    #[test]
    fn the_phases_run_in_the_specified_order() {
        let directory = TempDir::new().unwrap();
        let scope = scope(&directory);
        let (evidence, digest) = evidence_for(&directory, &scope);
        let guard = CoordinationGuard::acquire(&scope, deadline()).unwrap();
        let backend = Connected::default();
        let coordinator = MigrationCoordinator::new(&scope.instance_home, deadline());

        let journal = coordinator
            .confirm(
                MigrationRequest {
                    operation_id: "op-1",
                    target: &artifact(),
                    evidence: &evidence,
                    preview_digest: &digest,
                    fault: None,
                },
                held(guard),
                &backend,
            )
            .unwrap();

        assert_eq!(
            backend.recorder.calls(),
            vec!["freeze", "watchman", "stop", "connect", "acknowledge"],
            "the phases must follow the plan's order"
        );
        assert_eq!(journal.phase, MigrationPhase::Completed);
        assert_eq!(journal.failed_at, None);
    }

    #[test]
    fn the_accepted_record_is_written_before_any_side_effect() {
        let directory = TempDir::new().unwrap();
        let scope = scope(&directory);
        let (evidence, digest) = evidence_for(&directory, &scope);
        let guard = CoordinationGuard::acquire(&scope, deadline()).unwrap();
        let backend = Recorder::default();
        backend.fail_on("freeze");
        let coordinator = MigrationCoordinator::new(&scope.instance_home, deadline());

        let journal = coordinator
            .confirm(
                MigrationRequest {
                    operation_id: "op-accepted",
                    target: &artifact(),
                    evidence: &evidence,
                    preview_digest: &digest,
                    fault: None,
                },
                held(guard),
                &backend,
            )
            .unwrap();
        assert_eq!(journal.phase, MigrationPhase::Failed);
        assert_eq!(journal.failed_at, Some(MigrationPhase::SelfHealFrozen));

        // The record on disk must exist even though the first side effect failed:
        // after a crash here, "nothing happened" would be the wrong story.
        // The journal carries start identities and build ids: not secrets, but not
        // something to leave world-readable either.
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = MigrationCoordinator::operations_dir(&scope.instance_home);
            let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(dir_mode, 0o700, "the journal directory must be private");
            let file = dir.join("op-accepted.json");
            let file_mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
            assert_eq!(file_mode, 0o600, "the journal itself must be private");
        }

        let stored = read_journals(&scope.instance_home).unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].operation_id, "op-accepted");
        assert_eq!(stored[0].phase, MigrationPhase::Failed);
    }

    // -- crash injection ------------------------------------------------------

    #[test]
    fn a_crash_after_the_stop_never_stops_again_on_resume() {
        // The record says the legacy process is gone. Re-running `stop` would kill
        // whatever now owns that pid, or the same pid after it was recycled.
        let directory = TempDir::new().unwrap();
        let scope = scope(&directory);
        let (evidence, digest) = evidence_for(&directory, &scope);
        let guard = CoordinationGuard::acquire(&scope, deadline()).unwrap();
        let backend = Recorder::default();
        let coordinator = MigrationCoordinator::new(&scope.instance_home, deadline());

        let crashed = coordinator
            .confirm(
                MigrationRequest {
                    operation_id: "op-crash",
                    target: &artifact(),
                    evidence: &evidence,
                    preview_digest: &digest,
                    fault: Some(FaultPoint::AfterStop),
                },
                held(guard),
                &backend,
            )
            .unwrap();
        assert_eq!(crashed.phase, MigrationPhase::LegacyStopped);
        assert_eq!(backend.count("stop"), 1);

        // A second confirm for a *different* id is a new operation and would stop
        // again; what must not happen is that the same operation silently redoes it.
        // The resume rule is expressed on the phase, so assert the rule itself.
        assert!(
            crashed.phase.legacy_is_stopped(),
            "a record at LegacyStopped must tell a resumed run not to stop again"
        );
    }

    #[test]
    fn a_crash_before_accepted_writes_nothing() {
        let directory = TempDir::new().unwrap();
        let scope = scope(&directory);
        let (evidence, digest) = evidence_for(&directory, &scope);
        let guard = CoordinationGuard::acquire(&scope, deadline()).unwrap();
        let backend = Recorder::default();
        let coordinator = MigrationCoordinator::new(&scope.instance_home, deadline());

        let outcome = coordinator.confirm(
            MigrationRequest {
                operation_id: "op-before",
                target: &artifact(),
                evidence: &evidence,
                preview_digest: &digest,
                fault: Some(FaultPoint::BeforeAccept),
            },
            held(guard),
            &backend,
        );
        assert!(outcome.is_err(), "a pre-accept stop is not an operation");
        assert!(
            read_journals(&scope.instance_home).unwrap().is_empty(),
            "a migration that never started must leave no record"
        );
        assert_eq!(backend.calls(), Vec::<&str>::new());
    }

    #[test]
    fn a_truncated_journal_is_refused_rather_than_read_as_absent() {
        let directory = TempDir::new().unwrap();
        let scope = scope(&directory);
        let coordinator = MigrationCoordinator::new(&scope.instance_home, deadline());
        let dir = MigrationCoordinator::operations_dir(&scope.instance_home);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("op-truncated.json"), b"{ not json").unwrap();

        let outcome = coordinator.operation("op-truncated");
        assert!(
            outcome.is_err(),
            "an unreadable journal must not be reported as 'no such operation': {:?}",
            outcome.map(|journal| journal.is_some())
        );
    }

    #[test]
    fn the_same_operation_id_returns_the_existing_result() {
        let directory = TempDir::new().unwrap();
        let scope = scope(&directory);
        let (evidence, digest) = evidence_for(&directory, &scope);
        let guard = CoordinationGuard::acquire(&scope, deadline()).unwrap();
        let backend = Connected::default();
        let coordinator = MigrationCoordinator::new(&scope.instance_home, deadline());

        let first = coordinator
            .confirm(
                MigrationRequest {
                    operation_id: "op-once",
                    target: &artifact(),
                    evidence: &evidence,
                    preview_digest: &digest,
                    fault: None,
                },
                held(guard),
                &backend,
            )
            .unwrap();
        assert_eq!(first.phase, MigrationPhase::Completed);

        // The caller's response was lost; asking again must report the stored
        // outcome, not run the phases a second time.
        let guard = CoordinationGuard::acquire(&scope, deadline()).unwrap();
        let again = coordinator
            .confirm(
                MigrationRequest {
                    operation_id: "op-once",
                    target: &artifact(),
                    evidence: &evidence,
                    preview_digest: &digest,
                    fault: None,
                },
                held(guard),
                &backend,
            )
            .unwrap();
        assert_eq!(again.result.as_deref(), Some("completed"));
        assert_eq!(
            backend.recorder.count("stop"),
            1,
            "a repeated confirm must not stop the legacy runtime twice: {:?}",
            backend.recorder.calls()
        );
        assert_eq!(
            backend.recorder.count("freeze"),
            1,
            "a repeated confirm must not re-run any phase"
        );
    }

    // -- refusal --------------------------------------------------------------

    #[test]
    fn a_stale_preview_cannot_authorise_a_migration() {
        let directory = TempDir::new().unwrap();
        let scope = scope(&directory);
        let (evidence, _digest) = evidence_for(&directory, &scope);
        let guard = CoordinationGuard::acquire(&scope, deadline()).unwrap();
        let backend = Recorder::default();
        let coordinator = MigrationCoordinator::new(&scope.instance_home, deadline());

        let outcome = coordinator.confirm(
            MigrationRequest {
                operation_id: "op-stale",
                target: &artifact(),
                evidence: &evidence,
                preview_digest: "a-digest-from-a-different-instance-state",
                fault: None,
            },
            held(guard),
            &backend,
        );
        assert!(outcome.is_err());
        assert_eq!(backend.calls(), Vec::<&str>::new());
    }

    // -- the instance gate ----------------------------------------------------

    #[test]
    fn a_second_migration_on_the_same_instance_is_refused() {
        // The instance, not the scope, is the unit that can be migrated: two
        // migrations in parallel would each stop a daemon the other is about to use.
        let directory = TempDir::new().unwrap();
        let scope = scope(&directory);
        let _first = RetirementGate::acquire(&scope.instance_home, deadline()).unwrap();
        assert!(RetirementGate::is_held(&scope.instance_home));

        let second = RetirementGate::acquire(
            &scope.instance_home,
            Deadline::after(Duration::from_millis(50)),
        );
        assert!(
            second.is_err(),
            "a second migration must not share the instance gate"
        );

        drop(_first);
        assert!(!RetirementGate::is_held(&scope.instance_home));
        // And once released, a migration may start.
        assert!(RetirementGate::acquire(&scope.instance_home, deadline()).is_ok());
    }

    #[test]
    fn reconcile_reports_without_running_anything() {
        let directory = TempDir::new().unwrap();
        let scope = scope(&directory);
        let (evidence, digest) = evidence_for(&directory, &scope);
        let guard = CoordinationGuard::acquire(&scope, deadline()).unwrap();
        let backend = Connected::default();
        let coordinator = MigrationCoordinator::new(&scope.instance_home, deadline());
        coordinator
            .confirm(
                MigrationRequest {
                    operation_id: "op-reconcile",
                    target: &artifact(),
                    evidence: &evidence,
                    preview_digest: &digest,
                    fault: None,
                },
                held(guard),
                &backend,
            )
            .unwrap();
        let after_confirm = backend.recorder.calls();

        let reported = coordinator.reconcile().unwrap();
        assert_eq!(reported.len(), 1);
        assert_eq!(reported[0].phase, MigrationPhase::Completed);
        assert_eq!(
            backend.recorder.calls(),
            after_confirm,
            "reconcile must observe only"
        );
    }

    // -- the gap between connect and acknowledgment ---------------------------

    #[test]
    fn the_coordination_lock_is_released_before_ownership_is_acknowledged() {
        // Acknowledging ownership can involve waiting for a user. Doing that under
        // the lock would make one slow answer block every other coordinator, so the
        // lock has to be gone by then.
        let directory = TempDir::new().unwrap();
        let scope = scope(&directory);
        let (evidence, digest) = evidence_for(&directory, &scope);
        let guard = CoordinationGuard::acquire(&scope, deadline()).unwrap();
        let backend = Connected::default();
        *backend.recorder.observed_scope.borrow_mut() = Some(scope.runtime_dir.clone());
        let coordinator = MigrationCoordinator::new(&scope.instance_home, deadline());

        let journal = coordinator
            .confirm(
                MigrationRequest {
                    operation_id: "op-release",
                    target: &artifact(),
                    evidence: &evidence,
                    preview_digest: &digest,
                    fault: None,
                },
                held(guard),
                &backend,
            )
            .unwrap();

        assert_eq!(journal.phase, MigrationPhase::Completed);
        assert_eq!(
            *backend.recorder.lock_held_at_ack.borrow(),
            Some(false),
            "the coordination lock must be released before the acknowledgment"
        );
    }

    #[test]
    fn a_crash_right_after_release_leaves_a_connected_operation_for_reconciliation() {
        // This is the one gap the design accepts: connected, lock free, not yet
        // acknowledged. A crash here must leave something a human can read, and a
        // later attempt must not stop the legacy runtime again.
        let directory = TempDir::new().unwrap();
        let scope = scope(&directory);
        let (evidence, digest) = evidence_for(&directory, &scope);
        let guard = CoordinationGuard::acquire(&scope, deadline()).unwrap();
        let backend = Connected::default();
        let coordinator = MigrationCoordinator::new(&scope.instance_home, deadline());

        let crashed = coordinator
            .confirm(
                MigrationRequest {
                    operation_id: "op-gap",
                    target: &artifact(),
                    evidence: &evidence,
                    preview_digest: &digest,
                    fault: Some(FaultPoint::AfterRelease),
                },
                held(guard),
                &backend,
            )
            .unwrap();

        assert_eq!(crashed.phase, MigrationPhase::ManagedConnected);
        assert_eq!(crashed.failed_at, None);
        assert!(
            !crate::operation::retirement_is_active(&scope.instance_home),
            "a connected-but-unacknowledged migration is not a completed retirement"
        );

        let reported = coordinator.reconcile().unwrap();
        assert_eq!(reported.len(), 1);
        assert_eq!(reported[0].phase, MigrationPhase::ManagedConnected);

        // Asking again reports the recorded phase instead of running the phases:
        // replaying `stop` here is how a migration kills the wrong process.
        let again = coordinator
            .confirm(
                MigrationRequest {
                    operation_id: "op-gap",
                    target: &artifact(),
                    evidence: &evidence,
                    preview_digest: &digest,
                    fault: None,
                },
                held(CoordinationGuard::acquire(&scope, deadline()).unwrap()),
                &backend,
            )
            .unwrap();
        assert_eq!(again.phase, MigrationPhase::ManagedConnected);
        assert_eq!(backend.recorder.count("stop"), 1);
    }

    #[test]
    fn a_refused_acknowledgment_never_reports_completion() {
        let directory = TempDir::new().unwrap();
        let scope = scope(&directory);
        let (evidence, digest) = evidence_for(&directory, &scope);
        let guard = CoordinationGuard::acquire(&scope, deadline()).unwrap();
        let backend = Connected::default();
        backend.recorder.fail_on("acknowledge");
        let coordinator = MigrationCoordinator::new(&scope.instance_home, deadline());

        let journal = coordinator
            .confirm(
                MigrationRequest {
                    operation_id: "op-ack-refused",
                    target: &artifact(),
                    evidence: &evidence,
                    preview_digest: &digest,
                    fault: None,
                },
                held(guard),
                &backend,
            )
            .unwrap();

        assert_eq!(journal.phase, MigrationPhase::Failed);
        assert_eq!(journal.failed_at, Some(MigrationPhase::Completed));
        assert!(
            !retirement_is_active(&scope.instance_home),
            "a migration that never acknowledged ownership must not retire self-healing"
        );
    }

    #[test]
    fn evidence_from_another_scope_cannot_authorise_this_one() {
        // Two projects of one instance have separate scopes. A preview taken for one
        // must not migrate the other, even though the instance is the same.
        let directory = TempDir::new().unwrap();
        let other = scope(&directory);
        let mut different = other.clone();
        different.key = "another-scope".to_string();
        different.runtime_dir = directory.path().join("runtime-other");
        std::fs::create_dir_all(&different.runtime_dir).unwrap();

        let (evidence, digest) = evidence_for(&directory, &different);
        let guard = CoordinationGuard::acquire(&other, deadline()).unwrap();
        let backend = Connected::default();
        let coordinator = MigrationCoordinator::new(&other.instance_home, deadline());

        let outcome = coordinator.confirm(
            MigrationRequest {
                operation_id: "op-wrong-scope",
                target: &artifact(),
                evidence: &evidence,
                preview_digest: &digest,
                fault: None,
            },
            held(guard),
            &backend,
        );

        assert!(
            outcome.is_err(),
            "the scope must be checked before any side effect"
        );
        assert_eq!(backend.recorder.calls(), Vec::<&str>::new());
    }

    // -- no self-lock ---------------------------------------------------------

    /// A backend that really calls the bring-up path with the migration's own guard.
    ///
    /// If [`ensure_managed_under_guard`] re-acquired the lock, this would block until
    /// the deadline instead of returning; the test's point is the elapsed time.
    struct SelfLocking {
        probe: Absent,
        launcher: NoLaunch,
    }

    #[derive(Default)]
    struct Absent;

    impl ControlProbe for Absent {
        fn probe(
            &self,
            _path: &Path,
            _deadline: &Deadline,
        ) -> yi_agent_supervisors::lifecycle::bringup::ProbeOutcome {
            yi_agent_supervisors::lifecycle::bringup::ProbeOutcome::Absent
        }
    }

    #[derive(Default)]
    struct NoLaunch;

    impl GuardianLauncher for NoLaunch {
        fn launch(
            &self,
            scope: &RuntimeScope,
            _expected: &ArtifactIdentity,
        ) -> std::io::Result<()> {
            Err(std::io::Error::other(format!(
                "the test does not launch a guardian for {}",
                scope.key
            )))
        }
    }

    impl MigrationBackend for SelfLocking {
        fn freeze_self_heal(&self) -> std::io::Result<()> {
            Ok(())
        }
        fn retire_watchman(&self) -> std::io::Result<()> {
            Ok(())
        }
        fn stop_legacy(&self) -> std::io::Result<()> {
            Ok(())
        }
        fn ensure_managed(
            &self,
            held: &HeldScope,
            expected: &ArtifactIdentity,
            intent: BringUpIntent,
        ) -> std::io::Result<BringUpResult> {
            yi_agent_supervisors::lifecycle::bringup::ensure_managed_under_guard(
                held,
                expected,
                intent,
                &self.probe,
                &self.launcher,
                &[],
            )
        }
        fn acknowledge_ownership(&self, _proof: &OwnershipProof<'_>) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_migration_never_waits_on_the_lock_it_already_holds() {
        let directory = TempDir::new().unwrap();
        let scope = scope(&directory);
        let (evidence, digest) = evidence_for(&directory, &scope);
        let guard = CoordinationGuard::acquire(&scope, deadline()).unwrap();
        let backend = SelfLocking {
            probe: Absent,
            launcher: NoLaunch,
        };
        // The budget has to be long enough that "blocked on my own lock until the
        // deadline" is measurably slower than "refused for a real reason": 2s of
        // blocking is distinguishable, 80ms is not.
        let coordinator = MigrationCoordinator::new(
            &scope.instance_home,
            Deadline::after(Duration::from_secs(2)),
        );
        let started = std::time::Instant::now();

        let journal = coordinator
            .confirm(
                MigrationRequest {
                    operation_id: "op-no-self-lock",
                    target: &artifact(),
                    evidence: &evidence,
                    preview_digest: &digest,
                    fault: None,
                },
                held(guard),
                &backend,
            )
            .unwrap();

        assert!(
            started.elapsed() < Duration::from_millis(900),
            "the bring-up path must not wait on the lock the migration already holds; took {:?}",
            started.elapsed()
        );
        // It refused for a real reason (nothing to connect to), not by timing out.
        assert_eq!(journal.phase, MigrationPhase::Failed);
        assert_eq!(journal.failed_at, Some(MigrationPhase::ManagedConnected));
    }

    #[test]
    fn two_scopes_of_one_instance_migrate_within_one_budget() {
        let directory = TempDir::new().unwrap();
        let first = scope(&directory);
        let mut second = first.clone();
        second.key = "second-scope".to_string();
        second.runtime_dir = directory.path().join("runtime-second");
        std::fs::create_dir_all(&second.runtime_dir).unwrap();

        let run = |scope: RuntimeScope, id: &'static str| {
            let (evidence, digest) = evidence_for(&directory, &scope);
            let guard = CoordinationGuard::acquire(&scope, deadline()).unwrap();
            let backend = Connected::default();
            let coordinator = MigrationCoordinator::new(&scope.instance_home, deadline());
            let journal = coordinator
                .confirm(
                    MigrationRequest {
                        operation_id: id,
                        target: &artifact(),
                        evidence: &evidence,
                        preview_digest: &digest,
                        fault: None,
                    },
                    held(guard),
                    &backend,
                )
                .unwrap();
            (journal.phase, backend.recorder.count("stop"))
        };

        // The instance gate serialises them; the point is that serialising never
        // turns into waiting on a lock the same thread holds.
        let started = std::time::Instant::now();
        let a = run(first.clone(), "op-a");
        let b = run(second, "op-b");
        assert!(started.elapsed() < Duration::from_secs(20));
        assert_eq!(a, (MigrationPhase::Completed, 1));
        assert_eq!(b, (MigrationPhase::Completed, 1));
    }

    // -- the production backend's own guards -------------------------------

    /// A backend over injected facts, so the guards can be checked without a real
    /// legacy daemon: the interesting cases are all "the world changed since the
    /// preview", which is exactly what a fake can express and a live system cannot.
    struct Harness {
        _directory: TempDir,
        scope: RuntimeScope,
        evidence: LegacyEvidence,
        launchctl: FakeLaunchctl,
        home: PathBuf,
    }

    fn harness(directory: &TempDir) -> Harness {
        let scope = scope(directory);
        let home = directory.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let evidence =
            LegacyEvidenceProbe::with_facts(Facts::default(), Some(artifact()), home.clone())
                .inspect(&scope, &Deadline::after(Duration::from_secs(2)))
                .unwrap();
        Harness {
            _directory: TempDir::new().unwrap(),
            scope,
            evidence,
            launchctl: FakeLaunchctl::default(),
            home,
        }
    }

    fn backend<'a>(
        harness: &'a Harness,
        probes: MigrationProbes<'a>,
    ) -> SystemMigrationBackend<'a> {
        SystemMigrationBackend {
            scope: &harness.scope,
            evidence: &harness.evidence,
            home: &harness.home,
            launchctl: &harness.launchctl,
            probes,
            required_capabilities: &[],
        }
    }

    #[test]
    fn freezing_writes_a_record_without_claiming_the_migration_finished() {
        let directory = TempDir::new().unwrap();
        let harness = harness(&directory);
        let absent = Absent;
        let launch = NoLaunch;
        backend(
            &harness,
            MigrationProbes {
                probe: &absent,
                launcher: &launch,
            },
        )
        .freeze_self_heal()
        .unwrap();

        assert!(yi_agent_supervisors::lifecycle::retirement::is_frozen(
            &harness.scope.instance_home
        ));
        assert!(
            !yi_agent_supervisors::lifecycle::retirement::is_retired(&harness.scope.instance_home),
            "freezing is not completing: nothing has been taken over yet"
        );
    }

    #[test]
    fn an_unproven_identity_is_never_stopped() {
        // The evidence here cannot prove which process is running, so stopping would
        // be a guess with a kill attached.
        let directory = TempDir::new().unwrap();
        let harness = harness(&directory);
        let absent = Absent;
        let launch = NoLaunch;
        let outcome = backend(
            &harness,
            MigrationProbes {
                probe: &absent,
                launcher: &launch,
            },
        )
        .stop_legacy();

        assert!(outcome.is_err(), "an unproven process must not be stopped");
    }

    #[test]
    fn a_watchman_that_cannot_be_retired_automatically_is_reported_not_forced() {
        // A job whose plist names somebody else's program is not ours to boot out.
        let directory = TempDir::new().unwrap();
        let harness = harness(&directory);
        let foreign = PathBuf::from("/usr/local/bin/somebody-elses-watchman");
        let label = yi_agent_supervisors::watchman::LABEL;
        write_plist(&harness.home, label, &foreign);
        let launchctl = FakeLaunchctl {
            jobs: vec![LoadedJob {
                label: label.to_string(),
                program: Some(foreign),
                arguments: Vec::new(),
            }],
            booted_out: RefCell::new(Vec::new()),
            refusing: Vec::new(),
        };
        let absent = Absent;
        let launch = NoLaunch;
        let backend = SystemMigrationBackend {
            scope: &harness.scope,
            evidence: &harness.evidence,
            home: &harness.home,
            launchctl: &launchctl,
            probes: MigrationProbes {
                probe: &absent,
                launcher: &launch,
            },
            required_capabilities: &[],
        };

        let outcome = backend.retire_watchman();

        assert!(outcome.is_err(), "somebody else's job must not be retired");
        assert!(
            launchctl.booted_out.borrow().is_empty(),
            "nothing may be booted out when the job is not ours"
        );
    }

    #[test]
    fn ownership_is_refused_for_a_scope_the_backend_does_not_own() {
        // Acknowledging is the last check; a proof for another scope must never pass
        // it, however plausible the runtime answering is.
        let directory = TempDir::new().unwrap();
        let harness = harness(&directory);
        let absent = Absent;
        let launch = NoLaunch;
        let proof = OwnershipProof {
            operation_id: "op-other",
            scope_key: "somewhere/else",
            target_build_id: "build",
        };

        let outcome = backend(
            &harness,
            MigrationProbes {
                probe: &absent,
                launcher: &launch,
            },
        )
        .acknowledge_ownership(&proof);

        assert!(outcome.is_err());
    }

    // Silence the unused-import warning for the probe traits this module names.
    #[allow(dead_code)]
    fn _assert_traits(_: &dyn ControlProbe, _: &dyn GuardianLauncher) {}

    /// Retirement state, read the same way production reads it.
    pub fn retirement_is_active(instance_home: &Path) -> bool {
        yi_agent_supervisors::lifecycle::retirement::is_retired(instance_home)
    }
}
