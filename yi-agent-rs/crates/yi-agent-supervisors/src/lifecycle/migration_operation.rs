//! One migration, in observable steps, resumable after a crash.
//!
//! A migration is the only operation in this codebase that both stops something the
//! user is relying on and starts something new. Every intermediate state is
//! therefore a state another process can observe, and a crash in any of them must
//! leave an answer rather than a silent half-migration. That is what the journal is
//! for: each phase is written and fsynced *before* the side effect it describes, so
//! the record is always at or behind reality, never ahead of it.
//!
//! Two rules run through the whole module:
//!
//! - **Never retry a stop.** If the record says the legacy process was stopped, a
//!   resumed run does not stop anything again. A second stop of a recycled pid is
//!   how a migration kills an unrelated process.
//! - **Read-only on restart.** [`MigrationCoordinator::reconcile`] observes and
//!   reports; it never re-runs a phase. A human decides whether to retry.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use yi_agent_lifecycle::deadline::Deadline;
use yi_agent_lifecycle::types::{ArtifactIdentity, RuntimeScope};

use super::bringup::{BringUpIntent, BringUpResult, ControlProbe, GuardianLauncher, HeldScope};
use super::legacy_evidence::LegacyEvidence;
use super::retirement;

/// Where one migration has got to.
///
/// `ManagedConnected` is deliberately distinct from `Completed`: a managed runtime
/// can be connected while the business paths that depend on it are not ready yet, and
/// calling that "completed" would report success for work that cannot run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationPhase {
    /// The operation was accepted and recorded. Nothing has been changed yet.
    Accepted,
    /// The old self-healing loops are frozen.
    SelfHealFrozen,
    /// The old watchman was booted out and its absence verified.
    WatchmanRetired,
    /// The old daemon was stopped and its execution proved to have ended.
    LegacyStopped,
    /// The managed runtime is connected. Not yet business-ready.
    ManagedConnected,
    /// Ownership readiness was acknowledged for every required capability.
    Completed,
    /// The migration stopped at `failed_at`. Retirement stays active.
    Failed,
}

impl MigrationPhase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::SelfHealFrozen => "self_heal_frozen",
            Self::WatchmanRetired => "watchman_retired",
            Self::LegacyStopped => "legacy_stopped",
            Self::ManagedConnected => "managed_connected",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    /// Whether this phase has already performed the stop.
    ///
    /// The question a resumed run must ask before doing anything destructive: at or
    /// past this phase, the legacy process is already gone and stopping again would
    /// be stopping whatever now owns that pid.
    pub fn legacy_is_stopped(self) -> bool {
        matches!(
            self,
            Self::LegacyStopped | Self::ManagedConnected | Self::Completed
        )
    }
}

/// One journal record. Serialised as a single JSON object per operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationJournal {
    pub operation_id: String,
    pub scope_key: String,
    /// The runtime directory the operation acts on. Stored because an operation
    /// resumed from disk must not be pointed at whatever the caller names now.
    pub runtime_dir: PathBuf,
    /// The build the migration intended to connect to.
    pub target_build_id: String,
    /// Redacted digest: enough to tell evidence apart, useless to an attacker.
    pub evidence_digest: String,
    pub phase: MigrationPhase,
    /// The phase whose action failed, when `phase` is `Failed`.
    pub failed_at: Option<MigrationPhase>,
    /// One line, no capability and no key material.
    pub error: Option<String>,
    /// `Completed` results kept for replay, so a caller that lost the response gets
    /// the same answer instead of a second migration.
    pub result: Option<String>,
    pub updated_at_unix: u64,
}

/// Why a confirm was refused before anything was written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MigrationRefusal {
    /// The token does not match this evidence any more.
    StalePreview(String),
    /// The user confirmed a different scope than the one being migrated.
    WrongScope,
    /// A journal for this operation exists in a phase that cannot be resumed.
    Unresumable(String),
    /// The operation id is not a safe file name.
    UnsafeOperationId(String),
}

impl std::fmt::Display for MigrationRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StalePreview(reason) => write!(f, "the preview is stale: {reason}"),
            Self::WrongScope => write!(f, "the confirmation names a different scope"),
            Self::Unresumable(reason) => write!(f, "this operation cannot be resumed: {reason}"),
            Self::UnsafeOperationId(id) => write!(f, "unsafe operation id {id:?}"),
        }
    }
}

impl std::error::Error for MigrationRefusal {}

/// The side effects a migration performs, injectable so tests can fault-inject.
pub trait MigrationBackend {
    /// Freeze the old self-healing loops. Idempotent.
    fn freeze_self_heal(&self) -> std::io::Result<()>;
    /// Boot the old watchman out and verify it is gone.
    fn retire_watchman(&self) -> std::io::Result<()>;
    /// Stop the verified-idle legacy runtime.
    ///
    /// Called at most once per operation: the caller checks the journal first.
    fn stop_legacy(&self) -> std::io::Result<()>;
    /// Bring the managed runtime up under the guard this migration already holds.
    fn ensure_managed(
        &self,
        held: &HeldScope,
        expected: &ArtifactIdentity,
        intent: BringUpIntent,
    ) -> std::io::Result<BringUpResult>;
    /// Ask the frontend layer to acknowledge ownership readiness.
    ///
    /// Called *after* the coordination lock has been released, so the implementation
    /// must revalidate instead of assuming the world is unchanged: the proof carries
    /// what was connected, and the layer checks it still owns that generation.
    fn acknowledge_ownership(&self, proof: &OwnershipProof<'_>) -> std::io::Result<()>;
}

/// What the ownership acknowledgment is allowed to rely on.
///
/// The lock is gone by the time this is handed over, so "the operation I am
/// acknowledging" is a claim that must be checkable rather than an assumption. The
/// layer compares this against the runtime it actually attached to.
pub struct OwnershipProof<'a> {
    /// The operation whose `ManagedConnected` is being acknowledged.
    pub operation_id: &'a str,
    /// The scope recorded when the operation was accepted.
    pub scope_key: &'a str,
    /// The build that was connected, for a generation check.
    pub target_build_id: &'a str,
}

/// One loud, single reason the acknowledgment must not be trusted.
fn revalidate_after_release(
    stored: &MigrationJournal,
    operation_id: &str,
    scope_key: &str,
) -> Result<(), MigrationRefusal> {
    if stored.operation_id != operation_id {
        return Err(MigrationRefusal::Unresumable(format!(
            "the journal now describes {:?}, not {operation_id:?}",
            stored.operation_id
        )));
    }
    if stored.scope_key != scope_key {
        return Err(MigrationRefusal::WrongScope);
    }
    if stored.phase != MigrationPhase::ManagedConnected {
        return Err(MigrationRefusal::Unresumable(format!(
            "the operation is at {:?}, so it is not waiting for ownership readiness",
            stored.phase
        )));
    }
    Ok(())
}

/// One removable step, so a test can fail exactly here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultPoint {
    BeforeAccept,
    AfterAccept,
    BeforeFreeze,
    AfterFreeze,
    BeforeWatchman,
    AfterWatchman,
    BeforeStop,
    AfterStop,
    BeforeConnect,
    AfterConnect,
    /// After the record says `ManagedConnected` and the lock has been released.
    AfterRelease,
    BeforeComplete,
}

/// Records the migration, one fsynced phase at a time.
/// What the user is asking to migrate, and against which evidence.
///
/// One value because these always travel together into the same decision: the id names
/// the operation, and the digest is what the preview promised the user. Passing them
/// separately let a caller pair a digest with evidence it never described.
pub struct MigrationRequest<'a> {
    /// Caller-chosen, stable across retries of the same intent.
    pub operation_id: &'a str,
    /// The build the migrated runtime must be.
    pub target: &'a ArtifactIdentity,
    /// The snapshot the preview was shown for.
    pub evidence: &'a LegacyEvidence,
    /// The digest that preview handed to the user.
    pub preview_digest: &'a str,
    /// Stop after this point and return, as if the process had died. Tests only:
    /// production callers pass `None`.
    pub fault: Option<FaultPoint>,
}

pub struct MigrationCoordinator<'a> {
    instance_home: &'a Path,
    deadline: Deadline,
}

impl<'a> MigrationCoordinator<'a> {
    pub fn new(instance_home: &'a Path, deadline: Deadline) -> Self {
        Self {
            instance_home,
            deadline,
        }
    }

    /// Where the journal for this instance lives.
    ///
    /// A dedicated directory with 0700 permissions: the journal carries start
    /// identities and build ids, which are not secrets but are also not something to
    /// scatter across a home directory.
    pub fn operations_dir(instance_home: &Path) -> PathBuf {
        instance_home.join("migration-operations")
    }

    fn journal_path(&self, operation_id: &str) -> Result<PathBuf, MigrationRefusal> {
        Ok(Self::operations_dir(self.instance_home)
            .join(format!("{}.json", safe_component(operation_id)?)))
    }

    /// The operation this instance already has, if any.
    ///
    /// A second `confirm` for an id that exists returns the stored record instead of
    /// running the phases again: a lost response must not become a second migration.
    pub fn operation(&self, operation_id: &str) -> std::io::Result<Option<MigrationJournal>> {
        let path = self
            .journal_path(operation_id)
            .map_err(|refusal| std::io::Error::new(std::io::ErrorKind::InvalidInput, refusal))?;
        match std::fs::read(&path) {
            Ok(bytes) => {
                let journal: MigrationJournal = serde_json::from_slice(&bytes).map_err(|error| {
                    // A truncated journal is not "no journal": refusing is the only
                    // safe reading, because the phases it recorded may have happened.
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "the migration journal {} is unreadable ({error}); refusing to guess what happened",
                            path.display()
                        ),
                    )
                })?;
                Ok(Some(journal))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Read every operation this instance has recorded.
    pub fn operations(&self) -> std::io::Result<Vec<MigrationJournal>> {
        let dir = Self::operations_dir(self.instance_home);
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut journals = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let bytes = std::fs::read(&path)?;
            let journal: MigrationJournal = serde_json::from_slice(&bytes).map_err(|error| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "the migration journal {} is unreadable: {error}",
                        path.display()
                    ),
                )
            })?;
            journals.push(journal);
        }
        journals.sort_by(|left, right| left.operation_id.cmp(&right.operation_id));
        Ok(journals)
    }

    /// Reconcile after a restart: report what is recorded, change nothing.
    ///
    /// Deliberately not "resume": a phase boundary can only be crossed by a human
    /// deciding to, because the interesting crashes leave a state where the operator
    /// has to look at the machine.
    pub fn reconcile(&self) -> std::io::Result<Vec<MigrationJournal>> {
        self.operations()
    }

    /// Run the migration, in order, recording each phase before its side effect.
    ///
    /// `held` carries the scope guard the caller already owns. Taking it as one value
    /// means the guard and the scope cannot belong to different scopes.
    pub fn confirm(
        &self,
        request: MigrationRequest<'_>,
        mut held: HeldScope,
        backend: &dyn MigrationBackend,
    ) -> Result<MigrationJournal, MigrationRefusal> {
        let MigrationRequest {
            operation_id,
            target,
            evidence,
            preview_digest,
            fault,
        } = request;
        let scope = held.guard.scope();
        // A lost response must not become a second migration: an id that already has
        // a record returns that record, whatever phase it reached.
        if let Some(existing) = self.operation(operation_id).map_err(|error| {
            MigrationRefusal::Unresumable(format!(
                "the existing journal for {operation_id} cannot be read: {error}"
            ))
        })? {
            return Ok(existing);
        }
        if !evidence
            .still_matches(scope, preview_digest)
            .unwrap_or(false)
        {
            return Err(MigrationRefusal::StalePreview(
                "the evidence no longer hashes to the preview's digest".to_string(),
            ));
        }
        if self.deadline.is_expired() {
            return Err(MigrationRefusal::StalePreview(
                "the migration budget expired".to_string(),
            ));
        }

        let mut journal = MigrationJournal {
            operation_id: operation_id.to_string(),
            scope_key: scope.key.clone(),
            runtime_dir: scope.runtime_dir.clone(),
            target_build_id: target.build_id.clone(),
            evidence_digest: evidence.digest(),
            phase: MigrationPhase::Accepted,
            failed_at: None,
            error: None,
            result: None,
            updated_at_unix: now_unix(),
        };

        // The Accepted record is written *first*, before any side effect: a crash
        // after this point must not look like "nothing ever happened".
        if fault == Some(FaultPoint::BeforeAccept) {
            return Err(MigrationRefusal::Unresumable(
                "the fault injector stopped before anything was written".to_string(),
            ));
        }
        self.write(&journal)?;
        if fault == Some(FaultPoint::AfterAccept) {
            return Ok(journal);
        }

        // Phases run in the spec's order, and each one records its completion before
        // the next begins. `fail` keeps the record truthful about where it stopped.
        //
        // `Connect` is deliberately not in this slice: it is the last phase that may
        // hold the lock, and the lock has to be gone before ownership is
        // acknowledged. See the block below.
        for step in [Step::Freeze, Step::Watchman, Step::Stop] {
            if self.deadline.is_expired() {
                return Ok(self.fail(journal, step.phase(), "the migration budget expired", None));
            }
            match step.run_under_lock(backend, &held, target) {
                Ok(()) => {
                    journal.phase = step.phase();
                    journal.updated_at_unix = now_unix();
                    self.write(&journal)?;
                    // Only after the freeze marker is on disk may the instance gate
                    // go: until then a cooperating start that takes the gate would
                    // see no marker and legitimately start an embedded daemon, which
                    // is the very daemon this migration is replacing.
                    //
                    // Written *after* `self.write(&journal)?`, so a failed journal
                    // write leaves the gate held (RAII drops it on the error return)
                    // rather than opening the window.
                    if matches!(step, Step::Freeze) {
                        drop(held.retirement_gate.take());
                    }
                    if fault == Some(step.fault_after()) {
                        return Ok(journal);
                    }
                }
                Err(error) => {
                    let message = error.to_string();
                    return Ok(self.fail(
                        journal,
                        step.phase(),
                        &message,
                        Some(step.fault_before()),
                    ));
                }
            }
        }

        if self.deadline.is_expired() {
            return Ok(self.fail(
                journal,
                Step::Connect.phase(),
                "the migration budget expired",
                None,
            ));
        }
        let target_build_id = target.build_id.clone();
        let scope_key = journal.scope_key.clone();
        match Step::Connect.run_under_lock(backend, &held, target) {
            Ok(()) => {
                journal.phase = MigrationPhase::ManagedConnected;
                journal.updated_at_unix = now_unix();
                self.write(&journal)?;
            }
            Err(error) => {
                let message = error.to_string();
                return Ok(self.fail(
                    journal,
                    Step::Connect.phase(),
                    &message,
                    Some(Step::Connect.fault_before()),
                ));
            }
        }

        // ManagedConnected is not business-ready, and the acknowledgment that makes
        // it ready runs on the *other* side of the lock: holding the coordination
        // lock across a shell root or lease callback would block every other
        // coordinator for as long as a user takes to answer. So the lock is dropped
        // here, explicitly, and the phase recorded above is what a crash in the gap
        // leaves behind -- a state [`MigrationCoordinator::reconcile`] can report and
        // a human can decide about, never a reason to stop anything again.
        drop(held);
        if fault == Some(FaultPoint::AfterRelease) {
            return Ok(journal);
        }

        // Revalidate under no lock: this is a *recheck*, not a resumption. Another
        // operation may have written a later phase while the lock was free.
        let stored = match self.operation(operation_id) {
            Ok(Some(stored)) => stored,
            Ok(None) => {
                return Ok(self.fail(
                    journal,
                    MigrationPhase::ManagedConnected,
                    "the journal disappeared between connect and acknowledgment",
                    None,
                ));
            }
            Err(error) => {
                return Ok(self.fail(
                    journal,
                    MigrationPhase::ManagedConnected,
                    &format!("the journal could not be re-read: {error}"),
                    None,
                ));
            }
        };
        if let Err(refusal) = revalidate_after_release(&stored, operation_id, &scope_key) {
            return Ok(self.fail(
                stored,
                MigrationPhase::ManagedConnected,
                &refusal.to_string(),
                None,
            ));
        }
        if self.deadline.is_expired() {
            return Ok(self.fail(
                stored,
                MigrationPhase::Completed,
                "the migration budget expired before ownership was acknowledged",
                None,
            ));
        }

        let proof = OwnershipProof {
            operation_id,
            scope_key: &scope_key,
            target_build_id: &target_build_id,
        };
        if let Err(error) = backend.acknowledge_ownership(&proof) {
            let message = error.to_string();
            return Ok(self.fail(
                stored,
                MigrationPhase::Completed,
                &message,
                Some(FaultPoint::BeforeComplete),
            ));
        }
        journal = stored;
        journal.phase = MigrationPhase::Completed;
        journal.updated_at_unix = now_unix();

        // Retirement is recorded last: it is the statement "this instance no longer
        // restarts itself", and it must not be true before that is actually the case.
        if let Err(error) = retirement::retire(self.instance_home, "managed runtime took over") {
            return Ok(self.fail(
                journal,
                MigrationPhase::Completed,
                &format!("could not record the retirement: {error}"),
                None,
            ));
        }
        journal.result = Some("completed".to_string());
        self.write(&journal)?;
        Ok(journal)
    }

    fn fail(
        &self,
        mut journal: MigrationJournal,
        failed_at: MigrationPhase,
        error: &str,
        _fault: Option<FaultPoint>,
    ) -> MigrationJournal {
        journal.phase = MigrationPhase::Failed;
        journal.failed_at = Some(failed_at);
        // One line, no newline: the journal is a record, not a log.
        journal.error = Some(error.replace('\n', " "));
        journal.updated_at_unix = now_unix();
        // A failed write here must not hide the original failure from the caller, so
        // the error is only surfaced by the returned record.
        let _ = self.write(&journal);
        journal
    }

    /// Write one phase, atomically and durably.
    ///
    /// Temp-write, fsync the file, rename, fsync the directory. A journal that
    /// survived a crash while its rename was still in the page cache would describe a
    /// phase that may not have happened, which is the one direction that must never
    /// occur.
    fn write(&self, journal: &MigrationJournal) -> Result<(), MigrationRefusal> {
        let dir = Self::operations_dir(self.instance_home);
        std::fs::create_dir_all(&dir).map_err(|error| {
            MigrationRefusal::Unresumable(format!("cannot create {}: {error}", dir.display()))
        })?;
        set_dir_mode(&dir);
        let final_path = self.journal_path(&journal.operation_id)?;
        let temp_path = dir.join(format!(
            ".{}.{}.tmp",
            journal.operation_id,
            std::process::id()
        ));
        let body = serde_json::to_vec_pretty(journal).map_err(|error| {
            MigrationRefusal::Unresumable(format!("cannot encode the journal: {error}"))
        })?;
        {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .mode(0o600)
                .open(&temp_path)
                .map_err(|error| {
                    MigrationRefusal::Unresumable(format!(
                        "cannot write {}: {error}",
                        temp_path.display()
                    ))
                })?;
            file.write_all(&body)
                .and_then(|()| file.sync_all())
                .map_err(|error| {
                    MigrationRefusal::Unresumable(format!(
                        "cannot persist {}: {error}",
                        temp_path.display()
                    ))
                })?;
        }
        std::fs::rename(&temp_path, &final_path).map_err(|error| {
            MigrationRefusal::Unresumable(format!("cannot move the journal into place: {error}"))
        })?;
        // Parent directory fsync: on macOS a rename is durable only after this.
        if let Ok(dir_handle) = std::fs::File::open(&dir) {
            let _ = dir_handle.sync_all();
        }
        Ok(())
    }
}

/// One phase's work, named so the loop above reads as the spec's order.
#[derive(Clone, Copy)]
enum Step {
    Freeze,
    Watchman,
    Stop,
    Connect,
}

impl Step {
    fn phase(self) -> MigrationPhase {
        match self {
            Self::Freeze => MigrationPhase::SelfHealFrozen,
            Self::Watchman => MigrationPhase::WatchmanRetired,
            Self::Stop => MigrationPhase::LegacyStopped,
            Self::Connect => MigrationPhase::ManagedConnected,
        }
    }

    fn fault_before(self) -> FaultPoint {
        match self {
            Self::Freeze => FaultPoint::BeforeFreeze,
            Self::Watchman => FaultPoint::BeforeWatchman,
            Self::Stop => FaultPoint::BeforeStop,
            Self::Connect => FaultPoint::BeforeConnect,
        }
    }

    fn fault_after(self) -> FaultPoint {
        match self {
            Self::Freeze => FaultPoint::AfterFreeze,
            Self::Watchman => FaultPoint::AfterWatchman,
            Self::Stop => FaultPoint::AfterStop,
            Self::Connect => FaultPoint::AfterConnect,
        }
    }

    /// The phases that run while the caller still holds the coordination lock.
    fn run_under_lock(
        self,
        backend: &dyn MigrationBackend,
        held: &HeldScope,
        target: &ArtifactIdentity,
    ) -> std::io::Result<()> {
        match self {
            Self::Freeze => backend.freeze_self_heal(),
            Self::Watchman => backend.retire_watchman(),
            Self::Stop => backend.stop_legacy(),
            Self::Connect => match backend.ensure_managed(held, target, BringUpIntent::Business)? {
                BringUpResult::Connected { .. } => Ok(()),
                BringUpResult::VersionChoiceRequired { .. } => Err(std::io::Error::other(
                    "the managed runtime is a different build; the user must choose",
                )),
                BringUpResult::ManualActionRequired { reason } => {
                    Err(std::io::Error::other(reason))
                }
            },
        }
    }
}

/// Reject an operation id that could escape the journal directory.
///
/// The id comes from a caller and is used as a file name, so it is validated here
/// rather than sanitised: silently rewriting an id would make two different
/// operations look like the same one.
fn safe_component(operation_id: &str) -> Result<String, MigrationRefusal> {
    let safe = !operation_id.is_empty()
        && operation_id.len() <= 128
        && operation_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'));
    if safe {
        Ok(operation_id.to_string())
    } else {
        Err(MigrationRefusal::UnsafeOperationId(
            operation_id.to_string(),
        ))
    }
}

fn set_dir_mode(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// An instance-wide gate shared by every cooperating start and migration.
///
/// The instance is the unit that can be migrated, so it is the unit that must be
/// serialised: two scopes migrating in parallel would each stop a daemon the other
/// is about to connect to. Fixed inode, like the scope guard, so a process that
/// dies while holding it releases it with its descriptor.
pub struct RetirementGate {
    /// Held only for its descriptor: the `flock` lives as long as this file does,
    /// and the kernel releases it on exit even if the process dies unwinding.
    _file: std::fs::File,
    path: PathBuf,
}

impl RetirementGate {
    pub const FILE: &'static str = "instance-migration.lock";

    /// Take the gate, or say who holds it.
    pub fn acquire(instance_home: &Path, deadline: Deadline) -> std::io::Result<Self> {
        use std::os::unix::io::AsRawFd;
        std::fs::create_dir_all(instance_home)?;
        let path = instance_home.join(Self::FILE);
        loop {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(&path)?;
            let outcome = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if outcome == 0 {
                return Ok(Self { _file: file, path });
            }
            let error = std::io::Error::last_os_error();
            let contended = matches!(
                error.raw_os_error(),
                Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN
            );
            if !contended {
                return Err(error);
            }
            if deadline.is_expired() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    format!("another migration holds {} for this instance", Self::FILE),
                ));
            }
            std::thread::sleep(deadline.clamp(std::time::Duration::from_millis(20)));
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether a migration currently holds this instance's gate.
    pub fn is_held(instance_home: &Path) -> bool {
        use std::os::unix::io::AsRawFd;
        let path = instance_home.join(Self::FILE);
        let Ok(file) = std::fs::OpenOptions::new().read(true).open(&path) else {
            // No gate file means nobody ever took one, which is "not held".
            return false;
        };
        let outcome = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if outcome == 0 {
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
            return false;
        }
        true
    }
}

impl LegacyEvidence {
    /// Whether this evidence still describes `scope` and still hashes to `digest`.
    ///
    /// A token is only as good as the evidence behind it: this is the check that
    /// stops a preview from authorising a migration of whatever the instance looks
    /// like now.
    pub fn still_matches(
        &self,
        scope: &RuntimeScope,
        digest: &str,
    ) -> Result<bool, MigrationRefusal> {
        if self.scope_key != scope.key || self.instance_home != scope.instance_home {
            return Err(MigrationRefusal::WrongScope);
        }
        Ok(self.digest() == digest)
    }
}

/// The migration as it runs against a real machine.
///
/// Every destructive step is guarded by a *re-read* of the fact that justified it,
/// never by what the preview saw: a preview is a photograph, and the process behind
/// a pid can be replaced between the photograph and the stop.
pub struct SystemMigrationBackend<'a> {
    /// The canonical scope being migrated. Carried whole rather than as a key: a
    /// migration that rebuilt a scope out of a string would be guessing at a path.
    pub scope: &'a RuntimeScope,
    /// What the preview proved about the legacy runtime. Its `identity` is the
    /// authorisation to stop that process and nothing else.
    pub evidence: &'a LegacyEvidence,
    /// The user's home, for the watchman plists a retirement has to remove.
    pub home: &'a Path,
    /// The launchd side, injectable so tests never touch the real domain.
    pub launchctl: &'a dyn super::migration::LaunchctlBackend,
    /// The control channel and guardian launcher used by the managed bring-up.
    pub probes: MigrationProbes<'a>,
    /// Capabilities the frontend needs before it counts as ready.
    pub required_capabilities: &'a [String],
}

impl MigrationBackend for SystemMigrationBackend<'_> {
    fn freeze_self_heal(&self) -> std::io::Result<()> {
        // A *record*, not a lock: the loops that consult it may be in another
        // process. It is written before anything is stopped, which is what makes
        // "frozen" true for every later check regardless of how this process ends.
        retirement::freeze(
            self.scope.instance_home.as_path(),
            "a managed runtime migration is in progress",
        )
        .map(|_| ())
    }

    fn retire_watchman(&self) -> std::io::Result<()> {
        // The plan re-verifies that a loaded job is ours (label *and* program *and*
        // plist) before booting it out, and refuses rather than guessing when they
        // disagree. Its own self-heal flag is supplied here because this step *is*
        // the freeze -- asking evidence for it would make the two steps require each
        // other forever.
        let mut probe = self.evidence.probe();
        probe.self_heal_frozen = retirement::is_frozen(&self.scope.instance_home);
        let mut plan =
            super::migration::MigrationPlan::inspect_for_home(self.scope, probe, self.home);
        let disposition = plan.execute(
            &self.scope.key,
            self.launchctl,
            &expected_program(self.evidence),
            Deadline::after(std::time::Duration::from_secs(20)),
        )?;
        match disposition {
            super::migration::MigrationDisposition::ManagedReady => Ok(()),
            super::migration::MigrationDisposition::ManualActionRequired => {
                Err(std::io::Error::other(
                    "the watchman cannot be retired automatically; a human has to act",
                ))
            }
            super::migration::MigrationDisposition::ExplicitTemporaryUse => Err(
                std::io::Error::other("a temporary-use choice cannot complete a retirement"),
            ),
        }
    }

    fn stop_legacy(&self) -> std::io::Result<()> {
        let Some(proven) = self.evidence.verified(&self.evidence.identity) else {
            // No proof of *which* process: stopping by pid alone would be a guess
            // with a kill attached.
            return Err(std::io::Error::other(
                "the legacy runtime's identity was never proven, so it cannot be stopped",
            ));
        };
        // Re-ask the kernel. The preview may be minutes old, and a recycled pid is a
        // different process that this migration has no business touching.
        self.still_is(&proven.pid, &proven.start_identity, &proven.executable)?;

        crate::daemon::stop(&self.project()).map_err(std::io::Error::other)?;

        // An acknowledged stop is not a proof of exit. The kernel lock has to be free
        // and the pid has to be gone before the journal may say the runtime stopped:
        // a daemon that answered `Stop` but is still unwinding would otherwise be
        // recorded as gone while it still holds the database.
        let deadline = Deadline::after(std::time::Duration::from_secs(10));
        loop {
            let lock_free = match super::legacy_evidence::probe_lock(
                &self.scope.runtime_dir.join("runtime.lock"),
            ) {
                Ok(held) => !held,
                // Cannot read the lock: unproven is not free.
                Err(_) => false,
            };
            let process_gone = super::legacy_evidence::process_identity_of(proven.pid).is_err();
            if lock_free && process_gone {
                return Ok(());
            }
            if deadline.is_expired() {
                return Err(std::io::Error::other(format!(
                    "the legacy runtime for {} did not prove it stopped (lock free: {lock_free}, \
                     process gone: {process_gone})",
                    self.scope.runtime_dir.display()
                )));
            }
            std::thread::sleep(deadline.clamp(std::time::Duration::from_millis(50)));
        }
    }

    fn ensure_managed(
        &self,
        held: &HeldScope,
        expected: &ArtifactIdentity,
        intent: BringUpIntent,
    ) -> std::io::Result<BringUpResult> {
        // The explicit business-aware entry rather than the forwarding wrapper: this
        // is the path a migration takes, and a migration that starts a second daemon
        // over a live business channel is the worst case of the A6 failure -- it can
        // lose the user's in-flight work while also breaking its own journal.
        super::bringup::ensure_managed_under_guard_with_business(
            held,
            expected,
            intent,
            self.probes.probe,
            &super::bringup::BusinessChannelProbe,
            self.probes.launcher,
            self.required_capabilities,
        )
    }

    fn acknowledge_ownership(&self, proof: &OwnershipProof<'_>) -> std::io::Result<()> {
        // The lock is gone by now, so "the runtime I connected is still the one this
        // operation owns" has to be re-read rather than assumed.
        if proof.scope_key != self.scope.key {
            return Err(std::io::Error::other(format!(
                "an operation for {} is being acknowledged against {}",
                proof.scope_key, self.scope.key
            )));
        }
        let path = super::bringup::control_path(self.scope);
        let outcome = self
            .probes
            .probe
            .probe(&path, &Deadline::after(std::time::Duration::from_secs(5)));
        match outcome {
            super::bringup::ProbeOutcome::Answered(identity) => {
                if identity.scope_key != proof.scope_key {
                    return Err(std::io::Error::other(format!(
                        "{} answered for {}, not {}",
                        path.display(),
                        identity.scope_key,
                        proof.scope_key
                    )));
                }
                if identity.mode != yi_agent_lifecycle::types::RuntimeMode::Managed {
                    return Err(std::io::Error::other(
                        "the runtime answering is not managed, so ownership was never taken",
                    ));
                }
                if identity.daemon_build_id.as_deref() != Some(proof.target_build_id) {
                    return Err(std::io::Error::other(format!(
                        "the runtime is build {:?}, not the {} this operation connected",
                        identity.daemon_build_id, proof.target_build_id
                    )));
                }
                Ok(())
            }
            super::bringup::ProbeOutcome::Absent
            | super::bringup::ProbeOutcome::Unresponsive(_) => Err(std::io::Error::other(format!(
                "the managed runtime for {} stopped answering before ownership was \
                     acknowledged",
                proof.scope_key
            ))),
        }
    }
}

impl SystemMigrationBackend<'_> {
    /// The project this scope serves, as the daemon API names it.
    fn project(&self) -> PathBuf {
        PathBuf::from(&self.scope.project_key)
    }

    /// Refuse unless `pid` is still the process the preview proved.
    fn still_is(&self, pid: &u32, start_identity: &str, executable: &Path) -> std::io::Result<()> {
        let (now_start, now_executable) = super::legacy_evidence::process_identity_of(*pid)
            .map_err(|reason| {
                std::io::Error::other(format!("pid {pid} cannot be identified any more: {reason}"))
            })?;
        if now_start != start_identity || now_executable != executable {
            return Err(std::io::Error::other(format!(
                "pid {pid} is no longer the runtime the preview described"
            )));
        }
        Ok(())
    }
}

/// The program a watchman job has to name before it may be booted out.
fn expected_program(evidence: &LegacyEvidence) -> PathBuf {
    evidence
        .verified(&evidence.identity)
        .map(|identity| identity.executable.clone())
        .unwrap_or_default()
}

/// Reads the migration journal for reporting, without a coordinator.
pub fn read_journals(instance_home: &Path) -> std::io::Result<Vec<MigrationJournal>> {
    MigrationCoordinator::new(
        instance_home,
        Deadline::after(std::time::Duration::from_secs(1)),
    )
    .operations()
}

/// The probes a migration needs from the runtime layer, grouped so one backend value
/// can be reused across phases.
pub struct MigrationProbes<'a> {
    pub probe: &'a dyn ControlProbe,
    pub launcher: &'a dyn GuardianLauncher,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_operation_id_cannot_escape_the_journal_directory() {
        assert!(safe_component("../escape").is_err());
        assert!(safe_component("a/b").is_err());
        assert!(safe_component("").is_err());
        assert!(safe_component("op-1").is_ok());
    }

    #[test]
    fn a_phase_at_or_past_the_stop_knows_the_legacy_process_is_gone() {
        assert!(!MigrationPhase::Accepted.legacy_is_stopped());
        assert!(!MigrationPhase::SelfHealFrozen.legacy_is_stopped());
        assert!(!MigrationPhase::WatchmanRetired.legacy_is_stopped());
        assert!(MigrationPhase::LegacyStopped.legacy_is_stopped());
        assert!(MigrationPhase::Completed.legacy_is_stopped());
    }
}
