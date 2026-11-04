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
