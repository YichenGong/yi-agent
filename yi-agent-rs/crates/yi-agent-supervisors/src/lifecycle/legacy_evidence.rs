//! What an old runtime installation actually *is*, observed rather than assumed.
//!
//! `migration.rs`decides what to do, but it takes a [`LegacyProbe`] whose booleans
//! are supplied by the caller. That is fine for the decision algebra and useless as
//! evidence: a bool cannot distinguish "checked and true" from "never checked", and
//! the second one is exactly what a migration must not act on.
//!
//! So every observation here is tri-state ([`Observation`]). A fact the platform
//! cannot prove is `Unknown` with the missing fact named, which makes the preview
//! say *why* it cannot proceed instead of presenting a guess as a clean bill.
//!
//! Everything in this module is read-only. `inspect` opens no channel that mutates
//! state, writes no file, and stops nothing: it runs on a UI poll, and a migration
//! that acted while merely being displayed would migrate a user who never asked.
//!
//! [`LegacyProbe`]: super::migration::LegacyProbe

use std::collections::BTreeSet;
use std::io;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use yi_agent_lifecycle::deadline::Deadline;
use yi_agent_lifecycle::scope::control_path;
use yi_agent_lifecycle::types::{ArtifactIdentity, RuntimeIdentity, RuntimeMode, RuntimeScope};

use super::migration::{
    LaunchctlBackend, LegacyProbe, LegacyWatchman, LoadedJob, SystemLaunchctl, plist_names_program,
};
use super::preview::CoordinatorImpact;

/// One observed fact.
///
/// Deliberately not `bool`: "verified false" and "could not check" lead to opposite
/// decisions, and collapsing them is how a migration starts a second daemon beside
/// a runtime it never proved was idle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Observation<T> {
    /// The platform answered, and this is what it said.
    Verified(T),
    /// Nobody could prove it, and this is the fact that is missing.
    Unknown(String),
    /// The platform answered, and the answer contradicts a safe takeover.
    Contradicted(String),
}

impl<T> Observation<T> {
    pub fn is_verified(&self) -> bool {
        matches!(self, Self::Verified(_))
    }

    /// What was asked, when the answer is not a usable fact.
    pub fn missing(&self) -> Option<&str> {
        match self {
            Self::Verified(_) => None,
            Self::Unknown(reason) | Self::Contradicted(reason) => Some(reason),
        }
    }
}

/// How a running old runtime proved its identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProvenIdentity {
    pub pid: u32,
    pub uid: u32,
    /// Kernel-reported start time, not the PID: the OS recycles PIDs, so a number
    /// that matches means nothing without this.
    pub start_identity: String,
    pub executable: PathBuf,
    pub executable_sha256: String,
    pub build_id: String,
}

/// One resurrection path the old install could use.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ResurrectionSource {
    /// A launchd job holds the old binary and would restart it.
    LaunchdJob { label: String, program: PathBuf },
    /// An external watchman loop ensures a daemon per registered project.
    WatchmanLoop { label: String },
    /// The old runtime is the host process itself, so stopping it stops the editor.
    EmbeddedInHost,
}

impl ResurrectionSource {
    pub fn describe(&self) -> String {
        match self {
            Self::LaunchdJob { label, program } => {
                format!("launchd job {label} runs {}", program.display())
            }
            Self::WatchmanLoop { label } => format!("watchman loop {label} is loaded"),
            Self::EmbeddedInHost => "the old runtime lives inside the frontend process".to_string(),
        }
    }
}

/// A registration that a migration would have to preserve or re-point.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct AffectedRegistration {
    pub project: PathBuf,
    /// Which requester holds it, so "both projects" is visible without guessing.
    pub requester: String,
}

/// Everything the probe learned about one instance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyEvidence {
    pub scope_key: String,
    pub instance_home: PathBuf,
    /// `Verified` only when the running runtime answered *and* its process identity
    /// and executable digest matched what was reported. A runtime that answers but
    /// cannot be matched to a process is `Unknown`, never `Verified`.
    pub identity: Observation<ProvenIdentity>,
    /// Whether the peer of the control socket runs as this user.
    pub peer_uid: Observation<u32>,
    /// Whether this instance's runtime lock is provably held or provably free.
    pub runtime_lock: Observation<bool>,
    /// What kind of runtime the old install runs.
    pub host_mode: Observation<RuntimeMode>,
    /// The protocol/schema pair the old runtime advertises.
    pub protocol: Observation<(u32, u32)>,
    /// Whether the old host can freeze admission and drain.
    pub admission_freeze: Observation<bool>,
    /// Every resurrection path found; `Unknown` when any is unaccounted for.
    pub resurrection: Observation<Vec<ResurrectionSource>>,
    /// Every registration in this instance the migration would touch.
    pub registrations: Observation<Vec<AffectedRegistration>>,
    /// Names the instance-home layout version this evidence was read under, so a
    /// stale preview cannot be applied to a re-laid-out home.
    pub instance_revision: String,
}

impl LegacyEvidence {
    /// Whether a takeover may proceed without a human.
    ///
    /// The conjunction is the whole policy: every one of these is a separate way to
    /// destroy somebody's work, and a single unproven one means automatic takeover is
    /// not safe. As a consequence, no evidence gathered in this build can satisfy it
    /// yet — the cooperative-host handshake that proves "frozen and drained and idle"
    /// does not exist — and `automatic_takeover_allowed` therefore returns `false`
    /// with the missing facts listed by [`Self::blockers`]. That is the correct
    /// answer, not a placeholder.
    pub fn automatic_takeover_allowed(&self) -> bool {
        self.blockers().is_empty()
    }

    /// Why automatic takeover is not allowed, as displayable reasons.
    pub fn blockers(&self) -> Vec<String> {
        let mut blockers = Vec::new();
        for (name, missing) in [
            ("the running runtime's identity", self.identity.missing()),
            ("the socket peer's uid", self.peer_uid.missing()),
            ("the runtime lock owner", self.runtime_lock.missing()),
            ("the old host mode", self.host_mode.missing()),
            ("the old protocol and schema", self.protocol.missing()),
            ("admission freeze support", self.admission_freeze.missing()),
            ("the resurrection paths", self.resurrection.missing()),
            (
                "the registrations this instance holds",
                self.registrations.missing(),
            ),
        ] {
            if let Some(reason) = missing {
                blockers.push(format!("{name} could not be verified: {reason}"));
            }
        }
        if let Some(mode) = self.verified(&self.host_mode)
            && *mode != RuntimeMode::Managed
        {
            blockers.push(format!(
                "the old runtime runs in {} mode, which blocks automatic takeover",
                match mode {
                    RuntimeMode::LegacyIndependent => "legacy independent",
                    RuntimeMode::LegacyEmbedded => "legacy embedded (the host is the runtime)",
                    RuntimeMode::Unknown => "an unknown",
                    RuntimeMode::Managed => unreachable!(),
                }
            ));
        }
        if let Some(frozen) = self.verified(&self.admission_freeze)
            && !*frozen
        {
            blockers.push("the old host cannot freeze admission and drain".to_string());
        }
        if let Some(held) = self.verified(&self.runtime_lock)
            && *held
        {
            blockers.push("the old runtime still holds its instance lock".to_string());
        }
        if let Some(sources) = self.verified(&self.resurrection) {
            for source in sources {
                blockers.push(source.describe());
            }
        }
        blockers
    }

    /// The stable digest a preview binds its token to.
    ///
    /// Covers every observation, so any change between preview and confirm
    /// invalidates the token: a PID that got recycled, a job that got loaded, a
    /// second project registered.
    pub fn digest(&self) -> String {
        let rendered = format!(
            "{}\u{1f}{}\u{1f}{:?}\u{1f}{:?}\u{1f}{:?}\u{1f}{:?}\u{1f}{:?}\u{1f}{:?}\u{1f}{:?}\u{1f}{:?}\u{1f}{}",
            self.scope_key,
            self.instance_home.display(),
            self.identity,
            self.peer_uid,
            self.runtime_lock,
            self.host_mode,
            self.protocol,
            self.admission_freeze,
            self.resurrection,
            self.registrations,
            self.instance_revision,
        );
        let mut hasher = Sha256::new();
        hasher.update(rendered.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    /// The same evidence as the decision algebra in `migration.rs` expects.
    ///
    /// `self_heal_frozen` is deliberately always false: nothing in evidence proves
    /// the old loop has *already* been frozen, and asserting it from "a job is
    /// loaded" or from "no job is loaded" would both be guesses in opposite
    /// directions.
    pub fn probe(&self) -> LegacyProbe {
        LegacyProbe {
            mode: self
                .verified(&self.host_mode)
                .copied()
                .unwrap_or(RuntimeMode::Unknown),
            identity_verified: self.identity.is_verified(),
            influence_complete: matches!(&self.resurrection, Observation::Verified(sources) if sources.is_empty()),
            can_retire_without_host_exit: !matches!(
                self.verified(&self.host_mode),
                Some(RuntimeMode::LegacyEmbedded)
            ),
            self_heal_frozen: false,
        }
    }

    pub fn verified<'a, T>(&'a self, observation: &'a Observation<T>) -> Option<&'a T> {
        match observation {
            Observation::Verified(value) => Some(value),
            _ => None,
        }
    }
}

/// A read-only conclusion the UI may display, and that a later confirm must match.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationPreview {
    /// Opaque, short-lived, in memory only. Bound to the evidence digest so a token
    /// cannot be reused after the instance changed underneath it.
    token: String,
    evidence_digest: String,
    scope_key: String,
    instance_home: PathBuf,
    affected: Vec<AffectedRegistration>,
    blockers: Vec<String>,
    expires_at: Instant,
}

impl MigrationPreview {
    /// How long a preview stays usable.
    ///
    /// Short on purpose: the longer it lives, the longer a user could be looking at
    /// a snapshot of an instance that has since started work.
    pub const TTL: Duration = Duration::from_secs(120);

    /// Bound the preview to one body of evidence, now.
    pub fn new(evidence: &LegacyEvidence) -> Self {
        let evidence_digest = evidence.digest();
        let token = opaque_token(&evidence_digest, &evidence.scope_key);
        Self {
            token,
            evidence_digest,
            scope_key: evidence.scope_key.clone(),
            instance_home: evidence.instance_home.clone(),
            affected: evidence
                .verified(&evidence.registrations)
                .cloned()
                .unwrap_or_default(),
            blockers: evidence.blockers(),
            expires_at: Instant::now() + Self::TTL,
        }
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn evidence_digest(&self) -> &str {
        &self.evidence_digest
    }

    pub fn scope_key(&self) -> &str {
        &self.scope_key
    }

    pub fn instance_home(&self) -> &Path {
        &self.instance_home
    }

    pub fn affected(&self) -> &[AffectedRegistration] {
        &self.affected
    }

    pub fn blockers(&self) -> &[String] {
        &self.blockers
    }

    pub fn is_expired_at(&self, now: Instant) -> bool {
        now >= self.expires_at
    }

    pub fn is_expired(&self) -> bool {
        self.is_expired_at(Instant::now())
    }

    /// Whether this token may still authorise a confirm against fresh evidence.
    ///
    /// The digest is compared, not the token: a token presented against evidence that
    /// no longer hashes the same is a token for a different instance state.
    pub fn still_authorises(&self, evidence: &LegacyEvidence, now: Instant) -> bool {
        !self.is_expired_at(now)
            && self.evidence_digest == evidence.digest()
            && self.scope_key == evidence.scope_key
    }
}

/// A token that names nothing an attacker could guess from the instance path.
fn opaque_token(digest: &str, scope_key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"yi-agent/migration-preview");
    hasher.update(digest.as_bytes());
    hasher.update(scope_key.as_bytes());
    hasher.update(std::process::id().to_le_bytes());
    let bytes = hasher.finalize();
    bytes[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The read-only platform reads, injectable so tests never touch the real system.
pub trait PlatformFacts {
    /// The uid of the peer that owns `socket`, as the kernel reports it.
    fn peer_uid(&self, socket: &Path) -> Observation<u32>;

    /// The kernel's pid for the peer holding `socket`.
    ///
    /// Separately injectable from the uid because the two can disagree, and the
    /// disagreement is itself evidence: a socket owned by this user but held by a
    /// process the kernel will not describe is not the runtime we think it is.
    fn peer_pid(&self, socket: &Path) -> Observation<u32>;

    /// Kernel-reported start identity and executable for `pid`.
    fn process_identity(&self, pid: u32) -> Observation<(String, PathBuf)>;

    /// The sha256 of a file's bytes.
    fn file_sha256(&self, path: &Path) -> Observation<String>;

    /// The registrations this instance home holds.
    fn registrations(&self, instance_home: &Path) -> Observation<Vec<AffectedRegistration>>;

    /// The launchd jobs that are loaded, as the system reports them.
    fn loaded_jobs(&self, labels: &[String]) -> Observation<Vec<LoadedJob>>;

    /// What the running runtime says about itself on its control socket.
    ///
    /// A fact like any other: it is read from outside this module, and a probe that
    /// cannot read it has not learned the answer.
    fn reported_identity(&self, socket: &Path, deadline: &Deadline)
    -> Observation<RuntimeIdentity>;
}

/// The real reads on this platform.
pub struct SystemFacts {
    launchctl: SystemLaunchctl,
}

impl Default for SystemFacts {
    fn default() -> Self {
        Self {
            launchctl: SystemLaunchctl,
        }
    }
}

impl PlatformFacts for SystemFacts {
    fn peer_uid(&self, socket: &Path) -> Observation<u32> {
        match peer_uid_of(socket) {
            Ok(uid) => Observation::Verified(uid),
            Err(reason) => Observation::Unknown(reason),
        }
    }

    fn peer_pid(&self, socket: &Path) -> Observation<u32> {
        match peer_pid_of(socket) {
            Ok(pid) => Observation::Verified(pid),
            Err(reason) => Observation::Unknown(reason),
        }
    }

    fn process_identity(&self, pid: u32) -> Observation<(String, PathBuf)> {
        match process_identity_of(pid) {
            Ok(identity) => Observation::Verified(identity),
            Err(reason) => Observation::Unknown(reason),
        }
    }

    fn file_sha256(&self, path: &Path) -> Observation<String> {
        match std::fs::read(path) {
            Ok(bytes) => {
                let mut hasher = Sha256::new();
                hasher.update(&bytes);
                Observation::Verified(format!("{:x}", hasher.finalize()))
            }
            Err(error) => Observation::Unknown(format!("cannot read {}: {error}", path.display())),
        }
    }

    fn registrations(&self, instance_home: &Path) -> Observation<Vec<AffectedRegistration>> {
        let Some(dir) = resident_dir_for(instance_home) else {
            return Observation::Verified(Vec::new());
        };
        let entries = yi_agent_store::resident::entries(&dir);
        // One entry per (project, requester): a project wanted by two plugins is two
        // registrations, and collapsing them would hide half the impact.
        let mut affected = BTreeSet::new();
        for entry in entries {
            for requester in entry.required_by {
                affected.insert(AffectedRegistration {
                    project: entry.project.clone(),
                    requester,
                });
            }
        }
        Observation::Verified(affected.into_iter().collect())
    }

    fn reported_identity(
        &self,
        socket: &Path,
        deadline: &Deadline,
    ) -> Observation<RuntimeIdentity> {
        if !socket.exists() {
            return Observation::Unknown(
                "no control socket exists, so no runtime reported an identity".to_string(),
            );
        }
        match read_identity(socket, deadline) {
            Ok(identity) => Observation::Verified(identity),
            Err(reason) => Observation::Unknown(reason),
        }
    }

    fn loaded_jobs(&self, labels: &[String]) -> Observation<Vec<LoadedJob>> {
        let mut jobs = Vec::new();
        for label in labels {
            match self.launchctl.loaded_job(label) {
                Ok(Some(job)) => jobs.push(job),
                Ok(None) => {}
                Err(error) => {
                    return Observation::Unknown(format!(
                        "cannot ask launchd about {label}: {error}"
                    ));
                }
            }
        }
        Observation::Verified(jobs)
    }
}

/// Reads one instance and hands back what it could prove.
pub struct LegacyEvidenceProbe<P> {
    facts: P,
    /// The build this caller expects, when it has one. Used to compare the running
    /// runtime's advertised identity against a known artifact.
    expected: Option<ArtifactIdentity>,
    home: PathBuf,
}

impl LegacyEvidenceProbe<SystemFacts> {
    /// A probe over the real platform.
    pub fn system(expected: Option<ArtifactIdentity>, home: impl Into<PathBuf>) -> Self {
        Self {
            facts: SystemFacts::default(),
            expected,
            home: home.into(),
        }
    }
}

impl<P: PlatformFacts> LegacyEvidenceProbe<P> {
    /// A probe over injected facts, for tests.
    pub fn with_facts(
        facts: P,
        expected: Option<ArtifactIdentity>,
        home: impl Into<PathBuf>,
    ) -> Self {
        Self {
            facts,
            expected,
            home: home.into(),
        }
    }

    /// Inspect one instance. Read-only: no writes, no bootout, no stop, and the
    /// control socket is only ever used for a read.
    ///
    /// A missing runtime directory is a fact, not an error: "nothing runs here" is
    /// exactly what a preview has to be able to show.
    pub fn inspect(&self, scope: &RuntimeScope, deadline: &Deadline) -> io::Result<LegacyEvidence> {
        // A directory that does not exist is a fact, not a failure: "nothing runs
        // here" is exactly what a preview has to be able to show. Whether a socket
        // is there is decided by the facts, not by this function -- asking the file
        // system here would bypass an injected source and make a test's answer
        // unreachable.
        let socket = control_path(&scope.runtime_dir);
        let reported = self.facts.reported_identity(&socket, deadline);

        let (identity, peer_uid, host_mode, protocol, admission_freeze) = match &reported {
            Observation::Verified(reported) => {
                let peer_uid = self.facts.peer_uid(&socket);
                let identity = self.prove_identity(reported, &socket, &scope.key, peer_uid.clone());
                let host_mode = Observation::Verified(reported.mode);
                let protocol = Observation::Verified((
                    reported.business_protocol.unwrap_or_default(),
                    reported.schema_min,
                ));
                // The advertised capabilities are the runtime's own claim. Only a
                // named capability counts: an absent one is unproven, not false.
                let admission_freeze = Observation::Verified(
                    reported
                        .capabilities
                        .iter()
                        .any(|capability| capability == "admission.freeze"),
                );
                (identity, peer_uid, host_mode, protocol, admission_freeze)
            }
            Observation::Unknown(reason) => (
                Observation::Unknown(reason.clone()),
                self.facts.peer_uid(&socket),
                Observation::Unknown(reason.clone()),
                Observation::Unknown(reason.clone()),
                Observation::Unknown(reason.clone()),
            ),
            Observation::Contradicted(reason) => (
                Observation::Contradicted(reason.clone()),
                self.facts.peer_uid(&socket),
                Observation::Contradicted(reason.clone()),
                Observation::Contradicted(reason.clone()),
                Observation::Contradicted(reason.clone()),
            ),
        };

        let runtime_lock = self.runtime_lock(&scope.runtime_dir);
        let resurrection = self.resurrection(scope, &host_mode);
        let registrations = self.facts.registrations(&scope.instance_home);

        Ok(LegacyEvidence {
            scope_key: scope.key.clone(),
            instance_home: scope.instance_home.clone(),
            identity,
            peer_uid,
            runtime_lock,
            host_mode,
            protocol,
            admission_freeze,
            resurrection,
            registrations,
            instance_revision: instance_revision(&scope.instance_home),
        })
    }

    /// Corroborate what the runtime *says* with what the OS says about its process.
    ///
    /// The peer uid alone is not identity: any process run by this user could hold
    /// that socket. So the pid the runtime reports must exist, must have the same
    /// start time and executable, and that executable's digest must equal the
    /// advertised one. When any step cannot be taken, the answer is `Unknown` -- a
    /// half-proved identity must never be upgraded to `Verified`.
    fn prove_identity(
        &self,
        reported: &RuntimeIdentity,
        socket: &Path,
        scope_key: &str,
        peer_uid: Observation<u32>,
    ) -> Observation<ProvenIdentity> {
        let Observation::Verified(peer_uid) = peer_uid else {
            return Observation::Unknown(
                "the socket peer uid is unknown, so the reported process cannot be attributed"
                    .to_string(),
            );
        };
        let our_uid = unsafe { libc::geteuid() };
        if peer_uid != our_uid {
            return Observation::Contradicted(format!(
                "the control socket is owned by uid {peer_uid}, not this user ({our_uid})"
            ));
        }
        // The pid comes from the kernel, never from the runtime: a process that names
        // its own pid could name a different, healthy one.
        let peer_pid = self.facts.peer_pid(socket);
        let Observation::Verified(pid) = peer_pid else {
            return Observation::Unknown(format!(
                "the kernel would not report the pid holding the control socket: {}",
                peer_pid.missing().unwrap_or("the runtime did not answer")
            ));
        };
        let Observation::Verified((start_identity, executable)) = self.facts.process_identity(pid)
        else {
            return Observation::Unknown(format!(
                "the kernel could not describe pid {pid}, so its identity is unproven"
            ));
        };
        let Observation::Verified(digest) = self.facts.file_sha256(&executable) else {
            return Observation::Unknown(format!(
                "the bytes of {} could not be read, so its digest is unproven",
                executable.display()
            ));
        };
        // Each of these compares the runtime's claim against the kernel's answer, so
        // a mismatch is a contradiction rather than an absence of evidence.
        if let Some(expected) = &self.expected {
            if expected.sha256 != digest {
                return Observation::Contradicted(format!(
                    "the executable at {} has digest {digest}, but this build is {}",
                    executable.display(),
                    expected.sha256
                ));
            }
            if let Some(build_id) = &reported.daemon_build_id
                && build_id != &expected.build_id
            {
                return Observation::Contradicted(format!(
                    "the running build is {build_id}, but this caller expected {}",
                    expected.build_id
                ));
            }
        }
        if reported.scope_key != scope_key {
            return Observation::Contradicted(format!(
                "the runtime answers for scope {}, not {scope_key}",
                reported.scope_key
            ));
        }
        Observation::Verified(ProvenIdentity {
            pid,
            uid: peer_uid,
            start_identity,
            executable,
            executable_sha256: digest,
            build_id: reported
                .daemon_build_id
                .clone()
                .unwrap_or_else(|| "unreported".to_string()),
        })
    }

    /// Whether this instance's runtime lock is held, proved by trying to take it.
    ///
    /// Taking and releasing an advisory lock is not a write to the runtime's state:
    /// it observes the same kernel fact a starting daemon would. When the lock file
    /// is absent, nothing can be holding it, which is a proven `false`.
    fn runtime_lock(&self, runtime_dir: &Path) -> Observation<bool> {
        let path = runtime_dir.join("runtime.lock");
        if !path.exists() {
            return Observation::Verified(false);
        }
        match probe_lock(&path) {
            Ok(held) => Observation::Verified(held),
            Err(reason) => Observation::Unknown(reason),
        }
    }

    /// Every way the old install could come back.
    fn resurrection(
        &self,
        scope: &RuntimeScope,
        host_mode: &Observation<RuntimeMode>,
    ) -> Observation<Vec<ResurrectionSource>> {
        let watchman = LegacyWatchman::for_home(&self.home);
        let labels = vec![watchman.label.clone(), watchman.legacy_label.clone()];
        let jobs = match self.facts.loaded_jobs(&labels) {
            Observation::Verified(jobs) => jobs,
            Observation::Unknown(reason) => {
                return Observation::Unknown(format!(
                    "the loaded launchd jobs could not be listed, so a resurrection path may be missing: {reason}"
                ));
            }
            Observation::Contradicted(reason) => return Observation::Contradicted(reason),
        };

        let mut sources = BTreeSet::new();
        for job in &jobs {
            // Only a job that actually runs this install's program counts. A plist
            // that merely shares a label is somebody else's.
            let names_ours = match &job.program {
                Some(program) => {
                    plist_names_program(program, &self.expected_program(scope))
                        || job
                            .arguments
                            .iter()
                            .any(|argument| argument.contains("yi-agent"))
                }
                None => true,
            };
            if !names_ours {
                continue;
            }
            if labels.contains(&job.label) {
                sources.insert(ResurrectionSource::WatchmanLoop {
                    label: job.label.clone(),
                });
            } else if let Some(program) = &job.program {
                sources.insert(ResurrectionSource::LaunchdJob {
                    label: job.label.clone(),
                    program: program.clone(),
                });
            }
        }
        if matches!(
            host_mode,
            Observation::Verified(RuntimeMode::LegacyEmbedded)
        ) {
            sources.insert(ResurrectionSource::EmbeddedInHost);
        }
        Observation::Verified(sources.into_iter().collect())
    }

    /// The program this instance's daemon would run, when this caller knows it.
    fn expected_program(&self, _scope: &RuntimeScope) -> PathBuf {
        self.expected
            .as_ref()
            .map(|artifact| artifact.executable.clone())
            .unwrap_or_default()
    }
}

/// Names the instance-home layout the evidence was read under.
///
/// Cheap and stable: the layout version plus the file's own mtime would be finer,
/// but the revision only has to change when a stale preview could mean something
/// different -- and re-laying out a home is what does that.
fn instance_revision(instance_home: &Path) -> String {
    let marker = instance_home.join("instance.json");
    match std::fs::metadata(&marker) {
        Ok(metadata) => {
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|since| since.as_nanos())
                .unwrap_or_default();
            format!("layout1-{}-{modified}", metadata.len())
        }
        Err(_) => "layout1-unversioned".to_string(),
    }
}

fn resident_dir_for(instance_home: &Path) -> Option<PathBuf> {
    let dir = instance_home.join(".yi-agent").join("resident");
    if dir.is_dir() {
        return Some(dir);
    }
    // Older installs kept the registry beside the home itself.
    let legacy = instance_home.join("resident");
    legacy.is_dir().then_some(legacy)
}

/// Read one `Status` frame from the control socket.
///
/// A read, by contract: the only request this function can send is `Status`, which
/// registers nothing and takes no lease.
fn read_identity(socket: &Path, deadline: &Deadline) -> Result<RuntimeIdentity, String> {
    use std::io::{BufRead, BufReader, Write};
    use yi_agent_lifecycle::types::{ControlRequest, ControlResponse};

    let mut stream = UnixStream::connect(socket)
        .map_err(|error| format!("cannot connect to {}: {error}", socket.display()))?;
    let remaining = deadline.remaining();
    if remaining.is_zero() {
        return Err("the inspection budget expired before the socket answered".to_string());
    }
    stream
        .set_read_timeout(Some(remaining))
        .map_err(|error| format!("cannot bound the socket read: {error}"))?;
    let request = serde_json::to_vec(&ControlRequest::Identity)
        .map_err(|error| format!("cannot encode a status request: {error}"))?;
    stream
        .write_all(&request)
        .and_then(|()| stream.write_all(b"\n"))
        .and_then(|()| stream.flush())
        .map_err(|error| format!("cannot ask {} for its status: {error}", socket.display()))?;
    let mut line = String::new();
    let mut reader = BufReader::new(stream);
    reader
        .read_line(&mut line)
        .map_err(|error| format!("no status answer from {}: {error}", socket.display()))?;
    let response: ControlResponse = serde_json::from_str(line.trim()).map_err(|error| {
        format!(
            "the status answer from {} is not readable: {error}",
            socket.display()
        )
    })?;
    match response {
        ControlResponse::Identity(identity) => Ok(identity),
        other => Err(format!(
            "{} answered an identity request with a different response: {other:?}",
            socket.display()
        )),
    }
}

/// The kernel's uid for the peer of a unix socket.
///
/// `getpeereid` is BSD-only; Linux answers the same question through
/// `SO_PEERCRED`, which returns uid/gid/pid in one `ucred`. Both are wrapped here
/// so the rest of the module never has to know which platform it is on.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn peer_uid_of(socket: &Path) -> Result<u32, String> {
    #[cfg(target_os = "macos")]
    {
        let stream = UnixStream::connect(socket)
            .map_err(|error| format!("cannot connect to {}: {error}", socket.display()))?;
        let mut uid: libc::uid_t = 0;
        let mut gid: libc::gid_t = 0;
        let outcome = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
        if outcome != 0 {
            return Err(format!(
                "the kernel would not report the socket peer: {}",
                io::Error::last_os_error()
            ));
        }
        Ok(uid)
    }
    #[cfg(target_os = "linux")]
    {
        Ok(peer_credentials_of(socket)?.0)
    }
}

/// The kernel's pid for the peer of a unix socket.
///
/// This is the fact the runtime's own report cannot supply: `RuntimeIdentity` says
/// what the process *claims*, and the kernel says *which process* is holding the
/// socket. Identity is the agreement between the two, so both are needed.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn peer_pid_of(socket: &Path) -> Result<u32, String> {
    #[cfg(target_os = "macos")]
    {
        let stream = UnixStream::connect(socket)
            .map_err(|error| format!("cannot connect to {}: {error}", socket.display()))?;
        let mut pid: libc::pid_t = 0;
        let mut length = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
        let outcome = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                &mut pid as *mut _ as *mut libc::c_void,
                &mut length,
            )
        };
        if outcome != 0 || pid <= 0 {
            return Err(format!(
                "the kernel would not report the socket peer's pid: {}",
                io::Error::last_os_error()
            ));
        }
        Ok(pid as u32)
    }
    #[cfg(target_os = "linux")]
    {
        Ok(peer_credentials_of(socket)?.1)
    }
}

/// Linux: uid and pid of the peer that holds a unix socket, in one syscall.
///
/// `SO_PEERCRED` is the closest equivalent to `getpeereid` + `LOCAL_PEERPID`: the
/// kernel fills a `ucred` for the connected peer, so the two facts a caller needs
/// come from the same answer and cannot describe different processes.
#[cfg(target_os = "linux")]
fn peer_credentials_of(socket: &Path) -> Result<(u32, u32), String> {
    let stream = UnixStream::connect(socket)
        .map_err(|error| format!("cannot connect to {}: {error}", socket.display()))?;
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let outcome = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut credentials as *mut _ as *mut libc::c_void,
            &mut length,
        )
    };
    if outcome != 0 || credentials.pid <= 0 {
        return Err(format!(
            "the kernel would not report the socket peer: {}",
            io::Error::last_os_error()
        ));
    }
    Ok((credentials.uid, credentials.pid as u32))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn peer_uid_of(_socket: &Path) -> Result<u32, String> {
    Err("socket peer credentials are only implemented for macOS and Linux".to_string())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn peer_pid_of(_socket: &Path) -> Result<u32, String> {
    Err("socket peer credentials are only implemented for macOS and Linux".to_string())
}

/// Kernel-reported start identity and executable path for a pid.
/// The kernel's answer for one pid: its start identity and executable path.
///
/// Public because the migration has to re-ask the same question *after* a preview:
/// a pid that matches a recorded number proves nothing if the process behind the
/// number was replaced in between.
///
/// `Err` means "this pid cannot be identified", which callers read as "it is gone".
/// That is why the non-Linux/non-macOS branch must not pretend: it returns an error
/// naming the missing platform support, and `SystemFacts::process_identity` turns
/// that into `Unknown` rather than into a verified absence.
#[cfg(target_os = "macos")]
pub fn process_identity_of(pid: u32) -> Result<(String, PathBuf), String> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let read = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    if read <= 0 {
        return Err(format!(
            "the kernel would not describe pid {pid}: {}",
            io::Error::last_os_error()
        ));
    }
    let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let written = unsafe {
        libc::proc_pidpath(
            pid as libc::c_int,
            buffer.as_mut_ptr() as *mut libc::c_void,
            buffer.len() as u32,
        )
    };
    if written <= 0 {
        return Err(format!(
            "the kernel would not report the executable of pid {pid}"
        ));
    }
    buffer.truncate(written as usize);
    let executable = PathBuf::from(String::from_utf8_lossy(&buffer).into_owned());
    // Start time is the identity: the pid alone is recycled by the OS, so a number
    // that matches a previous reading means nothing.
    let start_identity = format!("{}.{:06}", info.pbi_start_tvsec, info.pbi_start_tvusec);
    Ok((start_identity, executable))
}

/// Linux: the same identity as [`process_identity_of`]'s macOS branch, read from
/// `/proc` instead of `proc_pidinfo`.
///
/// `starttime` (field 22 of `/proc/<pid>/stat`) is the kernel's birth stamp in
/// clock ticks since boot; like the macOS pair it is stable for one process and
/// differs for a recycled pid. [`crate::lifecycle::processes::ProcessBirth`] already
/// uses exactly that field, so this branch keeps the two modules' notion of "the
/// same process" identical.
#[cfg(target_os = "linux")]
pub fn process_identity_of(pid: u32) -> Result<(String, PathBuf), String> {
    let birth = crate::lifecycle::processes::ProcessBirth::of(pid as i32)
        .map_err(|error| format!("the kernel would not describe pid {pid}: {error}"))?;
    // A zombie keeps its `/proc` entry and its birth stamp but can never run
    // again, so it must read as gone rather than as a live identity.
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .map_err(|error| format!("the kernel would not describe pid {pid}: {error}"))?;
    let after_name = stat
        .rfind(')')
        .ok_or_else(|| format!("pid {pid} has a malformed /proc stat"))?;
    let state = stat[after_name + 1..]
        .split_whitespace()
        .next()
        .ok_or_else(|| format!("pid {pid} has no process state"))?;
    if state == "Z" {
        return Err(format!("process {pid} has exited"));
    }
    let executable = std::fs::read_link(format!("/proc/{pid}/exe")).map_err(|error| {
        format!("the kernel would not report the executable of pid {pid}: {error}")
    })?;
    Ok((birth.started_at().to_string(), executable))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn process_identity_of(_pid: u32) -> Result<(String, PathBuf), String> {
    Err("process identity is only implemented for macOS and Linux".to_string())
}

/// Whether another process holds the advisory lock on `path`.
///
/// Takes and immediately releases the lock, which is the only way to ask the kernel
/// the same question a starting daemon asks. It changes nothing the daemon wrote.
/// Whether an advisory lock on `path` is currently held by someone else.
///
/// Public for the same reason as [`process_identity_of`]: a migration has to re-ask
/// after it asked for a stop, and "the daemon said it stopped" is a claim, not a
/// release of the kernel lock.
pub fn probe_lock(path: &Path) -> Result<bool, String> {
    use std::os::unix::io::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    let outcome = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if outcome == 0 {
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        return Ok(false);
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN => Ok(true),
        _ => Err(format!(
            "the kernel would not say whether {} is locked: {error}",
            path.display()
        )),
    }
}

/// The registrations this instance holds, as one impact entry each.
///
/// `revision` stays `None`: the registration file has no revision counter, and
/// inventing one would make a stale preview look fresh. `None` is the honest
/// answer -- "this list is what the file said, not a fenced snapshot".
pub fn registration_impact(evidence: &LegacyEvidence) -> CoordinatorImpact {
    let Some(registrations) = evidence.verified_registrations() else {
        return CoordinatorImpact::unanswered();
    };
    CoordinatorImpact {
        revision: None,
        running: Vec::new(),
        queued: Vec::new(),
        plugins: registrations
            .iter()
            .map(|entry| yi_agent_lifecycle::types::ImpactEntry {
                kind: "registration".to_string(),
                id: entry.project.display().to_string(),
                owner: Some(entry.requester.clone()),
            })
            .collect(),
    }
}

impl LegacyEvidence {
    pub fn verified_registrations(&self) -> Option<&Vec<AffectedRegistration>> {
        match &self.registrations {
            Observation::Verified(registrations) => Some(registrations),
            _ => None,
        }
    }
}
