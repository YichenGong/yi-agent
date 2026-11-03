//! Retiring the self-healing paths that predate the managed runtime.
//!
//! Two things made the old installs dangerous to switch over, and neither is
//! visible from inside a new build:
//!
//! - **A loaded launchd job keeps the old binary alive.** If it is still loaded, a
//!   new daemon that assumes it owns the runtime is sharing it with a self-healing
//!   supervisor that will restart the old process the moment it exits. Reading the
//!   plist is not enough: the question is whether the *job* is loaded, and what it
//!   points at.
//! - **An old TUI *is* the runtime.** `LegacyEmbedded` daemons live inside the TUI
//!   process, so killing them means killing the user's editor session. No new build
//!   can retire that without the host's cooperation, and guessing otherwise is how a
//!   migration turns into a data-loss incident.
//!
//! Everything here is therefore two-phase by construction: `inspect` is read-only
//! and decides whether a migration is even thinkable, and `execute` refuses to do
//! anything that could destroy work without both an explicit confirmation string and
//! a probe that says it is safe.

use std::path::{Path, PathBuf};

use yi_agent_lifecycle::deadline::Deadline;
use yi_agent_lifecycle::types::{RuntimeMode, RuntimeScope};

/// What the old installation looks like, as observed — never as assumed.
///
/// Every field is a separate question because they fail independently: an install
/// can be identity-verified yet still incomplete, or complete yet impossible to
/// retire without taking the user's session with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyProbe {
    /// What kind of runtime the old install ran.
    pub mode: RuntimeMode,
    /// Whether the running old artifact could be identified (build id, executable
    /// hash). An unverified identity cannot be safely booted out: we would be
    /// guessing which process we are looking at.
    pub identity_verified: bool,
    /// Whether every way the old install can resurrect itself was found.
    ///
    /// A launchd job plus a resident-registration loop plus an app-server setting is
    /// three separate resurrection paths; missing one means "retired" is a lie.
    pub influence_complete: bool,
    /// Whether the old runtime can be stopped without the user's host exiting.
    ///
    /// False for `LegacyEmbedded`, where the runtime is the TUI process itself.
    pub can_retire_without_host_exit: bool,
    /// Whether the old self-healing loop has already been frozen.
    ///
    /// Retiring a still-self-healing install races its own supervisor: it can come
    /// back between "bootout" and "start the new daemon".
    pub self_heal_frozen: bool,
}

/// What the caller should do next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MigrationDisposition {
    /// Safe to take over; the managed runtime can be brought up.
    ManagedReady,
    /// The user explicitly chose to keep using the old runtime for now.
    ///
    /// It has to keep being shown as an exception: silently treating an explicit
    /// "not now" as success is how a manual install becomes invisible.
    ExplicitTemporaryUse,
    /// Nothing automatic is safe. A human must act.
    ManualActionRequired,
}

/// A launchd job as the system reports it.
///
/// The value is launchctl's own JSON so the caller can inspect `Label` and
/// `ProgramArguments` without this crate inventing a lossy schema for them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedJob {
    pub label: String,
    /// Absolute path of the program the job runs, when the job describes one.
    pub program: Option<PathBuf>,
    /// The job's own `ProgramArguments`, verbatim.
    pub arguments: Vec<String>,
}

/// The side effects a migration needs, injectable so no test touches launchd.
pub trait LaunchctlBackend {
    /// The loaded job for this label, if any. An unloaded job is `None`, not an
    /// error: "not loaded" is the state we want.
    fn loaded_job(&self, label: &str) -> std::io::Result<Option<LoadedJob>>;

    /// Boot the job out of the current user's launchd domain.
    fn bootout(&self, label: &str) -> std::io::Result<()>;
}

/// Which of the old watchman labels this install may be using.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyWatchman {
    /// The current label; see [`super::super::watchman::LABEL`].
    pub label: String,
    /// The pre-generalisation label, still loaded on machines that installed it.
    pub legacy_label: String,
    pub plist: PathBuf,
    pub legacy_plist: PathBuf,
}

impl LegacyWatchman {
    /// Both labels this install could be running under, plus where their plists
    /// live.
    pub fn for_home(home: &Path) -> Self {
        Self {
            label: crate::watchman::LABEL.to_string(),
            legacy_label: crate::watchman::LEGACY_LABEL.to_string(),
            plist: crate::watchman::plist_path(home),
            legacy_plist: home
                .join("Library")
                .join("LaunchAgents")
                .join(format!("{}.plist", crate::watchman::LEGACY_LABEL)),
        }
    }

    /// The plist that carries `label`, if it is one of ours.
    ///
    /// A migration deletes the plist for a label it retired; leaving the file
    /// behind means the next login installs the job again, so the retirement would
    /// last until the next reboot.
    pub fn plist_for(&self, label: &str) -> Option<&Path> {
        if label == self.label {
            Some(&self.plist)
        } else if label == self.legacy_label {
            Some(&self.legacy_plist)
        } else {
            None
        }
    }

    /// Whether a loaded job really belongs to *this* install.
    ///
    /// A job with the right label but another program is somebody else's: booting it
    /// out would stop a process this migration has no business touching.
    pub fn claims(job: &LoadedJob, expected_program: &Path) -> bool {
        job.program
            .as_deref()
            .is_some_and(|program| program == expected_program)
    }
}

/// Why a probe does not permit an automatic takeover.
///
/// Named reasons, not a bare boolean: the UI has to explain each one, and "not
/// ready" with no cause is indistinguishable from a bug.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MigrationBlocker {
    /// The runtime lives inside the user's host process.
    EmbeddedInHost,
    /// The old artifact could not be identified.
    UnverifiedIdentity,
    /// A resurrection path was not found, so "retired" cannot be promised.
    IncompleteInfluence,
    /// The self-healing loop is still active and could undo the migration.
    SelfHealingActive,
}

impl MigrationBlocker {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::EmbeddedInHost => {
                "the old runtime is embedded in the frontend that is still running"
            }
            Self::UnverifiedIdentity => "the running old artifact could not be identified",
            Self::IncompleteInfluence => {
                "not every way the old install restarts itself could be found"
            }
            Self::SelfHealingActive => "the old install still restarts itself",
        }
    }
}

/// One inspectable migration decision, with the reasons behind it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationPlan {
    scope_key: String,
    probe: LegacyProbe,
    blockers: Vec<MigrationBlocker>,
    /// Which watchman labels were observed loaded, for the explanation and for
    /// `execute`.
    loaded: Vec<String>,
    /// The labels and plists this install could be using.
    watchman: LegacyWatchman,
}

impl MigrationPlan {
    /// Decides whether a takeover is safe, *reading nothing and changing nothing*.
    ///
    /// Inspection has to stay free of side effects because it runs on a UI poll: a
    /// plan that booted a job out while merely being displayed would migrate a user
    /// who never asked.
    pub fn inspect(scope: &RuntimeScope, probe: LegacyProbe) -> Self {
        Self::inspect_for_home(scope, probe, &std::env::temp_dir())
    }

    /// Same inspection with an explicit install home, so a test can name the plists
    /// it is pretending to find.
    pub fn inspect_for_home(scope: &RuntimeScope, probe: LegacyProbe, home: &Path) -> Self {
        let mut blockers = Vec::new();
        if probe.mode == RuntimeMode::LegacyEmbedded || !probe.can_retire_without_host_exit {
            blockers.push(MigrationBlocker::EmbeddedInHost);
        }
        if !probe.identity_verified {
            blockers.push(MigrationBlocker::UnverifiedIdentity);
        }
        if !probe.influence_complete {
            blockers.push(MigrationBlocker::IncompleteInfluence);
        }
        if !probe.self_heal_frozen {
            blockers.push(MigrationBlocker::SelfHealingActive);
        }
        Self {
            scope_key: scope.key.clone(),
            probe,
            blockers,
            loaded: Vec::new(),
            watchman: LegacyWatchman::for_home(home),
        }
    }

    /// Same plan with the watchman labels that were observed loaded.
    ///
    /// Recorded rather than re-read: the load state can change between inspection
    /// and execution, and a migration should act on what it *explained*.
    pub fn with_loaded(mut self, labels: Vec<String>) -> Self {
        self.loaded = labels;
        self
    }

    pub fn probe(&self) -> &LegacyProbe {
        &self.probe
    }

    pub fn blockers(&self) -> &[MigrationBlocker] {
        &self.blockers
    }

    pub fn loaded_labels(&self) -> &[String] {
        &self.loaded
    }

    pub fn scope_key(&self) -> &str {
        &self.scope_key
    }

    /// The disposition implied by the probe alone.
    ///
    /// `ExplicitTemporaryUse` is *never* returned here: it is the user's answer, not
    /// an inference. A read-only inspection that guessed "the user wants to wait"
    /// would silently keep an unsafe install alive.
    pub fn disposition(&self) -> MigrationDisposition {
        if self.blockers.is_empty() {
            MigrationDisposition::ManagedReady
        } else {
            MigrationDisposition::ManualActionRequired
        }
    }

    /// Records the user's choice to keep the old runtime for now.
    ///
    /// It does not make the install safe; it records that the exception is
    /// deliberate, so the UI can keep showing it instead of claiming a successful
    /// migration.
    pub fn temporary_use(&mut self) -> MigrationDisposition {
        MigrationDisposition::ExplicitTemporaryUse
    }

    /// Performs the destructive part, but only under an explicit confirmation.
    ///
    /// The confirmation is the user's own words (the label or scope they were shown),
    /// not a boolean: a boolean is what a stray click produces, and this operation
    /// stops processes.
    pub fn execute(
        &mut self,
        confirmation: &str,
        launchctl: &dyn LaunchctlBackend,
        expected_program: &Path,
        deadline: Deadline,
    ) -> std::io::Result<MigrationDisposition> {
        if self.blockers.iter().any(|blocker| {
            // These two cannot be fixed by acting on launchd at all; refusing early
            // avoids booting out a job that changes nothing.
            matches!(
                blocker,
                MigrationBlocker::EmbeddedInHost | MigrationBlocker::UnverifiedIdentity
            )
        }) {
            return Ok(MigrationDisposition::ManualActionRequired);
        }
        if confirmation.trim() != self.scope_key {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "the confirmation does not name this runtime; refusing to stop anything",
            ));
        }
        if deadline.is_expired() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "no budget left for the migration",
            ));
        }

        // Two independent pieces of evidence, both required. A plist that merely
        // contains a path string proves nothing (the string could be a log path or an
        // argument), and a loaded job whose plist we cannot read gives no way to tell
        // ours from a stranger's. Only agreement between them is a claim we can act
        // on — and disagreement is a conflict to report, never a guess to resolve.
        let watchman = self.watchman.clone();
        for label in self.labels_in_order() {
            let job = launchctl.loaded_job(&label)?;
            let plist = watchman.plist_for(&label);
            let plist_claims =
                plist.is_some_and(|path| plist_names_program(path, expected_program));

            match (job, plist_claims) {
                (Some(job), true) if LegacyWatchman::claims(&job, expected_program) => {
                    launchctl.bootout(&label)?;
                    // Deleting the plist is part of retiring, not cleanup: launchd
                    // loads every plist in `LaunchAgents` at next login, so a surviving
                    // file puts the job back after the next reboot.
                    if let Some(path) = plist {
                        remove_plist(path)?;
                    }
                }
                // Loaded, but not provably ours: another install or another product
                // shares this label. Stopping it could take down a process this
                // migration has no business touching, so report and change nothing.
                (Some(_), _) => return Ok(MigrationDisposition::ManualActionRequired),
                // Not loaded, but a plist of ours is still on disk. Nothing to stop,
                // but the file must go or the next login resurrects the job.
                (None, true) => {
                    if let Some(path) = plist {
                        remove_plist(path)?;
                    }
                }
                // Not loaded and no plist of ours: either already retired, or an
                // unreadable plist we cannot claim.
                (None, false) => {
                    if plist.is_some_and(|path| path.exists()) {
                        // A plist exists but does not name our program: somebody
                        // else's file under our label. Deleting it would break their
                        // install.
                        return Ok(MigrationDisposition::ManualActionRequired);
                    }
                }
            }
        }

        // Booted out is not the same as retired. If a resurrection path is still
        // unaccounted for, the job can come back the moment this returns, and
        // reporting `ManagedReady` would hand the caller a runtime that is about to be
        // duplicated.
        if self.blockers.iter().any(|blocker| {
            matches!(
                blocker,
                MigrationBlocker::IncompleteInfluence | MigrationBlocker::SelfHealingActive
            )
        }) {
            return Ok(MigrationDisposition::ManualActionRequired);
        }
        Ok(MigrationDisposition::ManagedReady)
    }

    /// The labels to visit, in the order the user was told about them.
    ///
    /// When nothing was observed loaded, both standard labels are still visited: the
    /// load state can change between inspection and execution, and a job that
    /// appeared in that gap would otherwise be missed entirely.
    fn labels_in_order(&self) -> Vec<String> {
        let mut labels = Vec::new();
        for label in self
            .loaded
            .iter()
            .chain([&self.watchman.legacy_label, &self.watchman.label])
        {
            if !labels.contains(label) {
                labels.push(label.clone());
            }
        }
        labels
    }

    /// The plist paths whose presence means an old install may exist.
    pub fn plist_paths(&self) -> [&Path; 2] {
        [&self.watchman.legacy_plist, &self.watchman.plist]
    }
}

/// Removes a retired plist, treating "already gone" as the desired end state.
fn remove_plist(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Whether a plist at `path` describes the program at `expected`.
///
/// Used to answer "is this really our install" without running launchctl. A plist
/// that cannot be read is not evidence of anything, so the answer is `false`.
pub fn plist_names_program(path: &Path, expected: &Path) -> bool {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return false;
    };
    contents.contains(&expected.display().to_string())
}

/// The real launchd, read through `launchctl print`.
///
/// `print` is used rather than `list` because `list` truncates to the fields the
/// legacy API knows about, and `program` is exactly the field a migration has to be
/// sure of before stopping anything.
pub struct SystemLaunchctl;

impl LaunchctlBackend for SystemLaunchctl {
    fn loaded_job(&self, label: &str) -> std::io::Result<Option<LoadedJob>> {
        let output = std::process::Command::new("launchctl")
            .arg("print")
            .arg(format!("{}/{}", uid_domain(), label))
            .output()?;
        if !output.status.success() {
            // `print` fails for a job that is not loaded. That is the state a
            // migration wants, so it is `None` rather than an error — but only for a
            // job that is genuinely absent, which launchd reports as a non-zero exit
            // with "Could not find service" on stderr.
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("Could not find service") {
                return Ok(None);
            }
            return Err(std::io::Error::other(format!(
                "launchctl print {label} failed: {}",
                stderr.trim()
            )));
        }
        Ok(Some(parse_loaded_job(
            label,
            &String::from_utf8_lossy(&output.stdout),
        )))
    }

    fn bootout(&self, label: &str) -> std::io::Result<()> {
        let status = std::process::Command::new("launchctl")
            .arg("bootout")
            .arg(format!("{}/{}", uid_domain(), label))
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(std::io::Error::other(format!(
                "launchctl bootout {label} failed: {status}"
            )))
        }
    }
}

fn uid_domain() -> String {
    format!("gui/{}", unsafe { libc::getuid() })
}

/// Extracts the program path from `launchctl print` output.
///
/// The line is `program = /path`; `arguments = { ... }` can follow. Anything else is
/// left alone — this parser must not guess a program from a substitute field, since
/// the whole point is to avoid stopping a job we cannot identify.
fn parse_loaded_job(label: &str, printed: &str) -> LoadedJob {
    let mut program = None;
    let mut arguments = Vec::new();
    let mut in_arguments = false;
    for line in printed.lines() {
        let trimmed = line.trim();
        if in_arguments {
            if trimmed == "}" {
                in_arguments = false;
                continue;
            }
            if !trimmed.is_empty() {
                arguments.push(trimmed.to_string());
            }
            continue;
        }
        if let Some(value) = trimmed.strip_prefix("program = ") {
            program = Some(PathBuf::from(value.trim()));
        } else if trimmed == "arguments = {" {
            in_arguments = true;
        }
    }
    LoadedJob {
        label: label.to_string(),
        program,
        arguments,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The program line is what "is this ours" rests on, so it must be read exactly.
    #[test]
    fn the_program_line_is_parsed() {
        let printed = "gui/501/ai.yi-agent.resident-daemon-watchman = {\n\tpath = /x.plist\n\tprogram = /usr/local/bin/yi-agent\n\targuments = {\n\t\t\tyi-agent\n\t\t\tresident\n\t\t\twatch\n\t}\n}\n";
        let job = parse_loaded_job("ai.yi-agent.resident-daemon-watchman", printed);
        assert_eq!(job.program, Some(PathBuf::from("/usr/local/bin/yi-agent")));
        assert_eq!(job.arguments, ["yi-agent", "resident", "watch"]);
    }

    /// No `program` line means no claim: an unknown program must never be assumed to
    /// be ours.
    #[test]
    fn a_missing_program_line_claims_nothing() {
        let printed = "gui/501/x = {\n\tstate = running\n}\n";
        let job = parse_loaded_job("x", printed);
        assert_eq!(job.program, None);
        assert!(job.arguments.is_empty());
    }
}
