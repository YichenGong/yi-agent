//! The one way a runtime comes up.
//!
//! Every frontend used to start its own daemon its own way: the TUI embedded one,
//! the app-server spanwed a detached one, a plugin install started another. Each
//! path had its own idea of "already running" and its own idea of what to do when
//! it was not, so two clients opening at once could each decide the other was
//! absent and start a second daemon on one database.
//!
//! This module is the single answer. Its shape is deliberately boring:
//!
//! 1. **Probe before deciding.** Under the coordination lock, ask the control
//!    channel who it is. A timeout is not "absent" — it is "unknown", and unknown
//!    never starts anything.
//! 2. **Inspect never starts.** `Inspect` exists so a UI can ask "what is running"
//!    without the act of asking changing the answer.
//! 3. **Compatibility is a classification, not a boolean.** "Different build but
//!    the same business protocol" and "old process with no lifecycle guarantees"
//!    need different user decisions, and collapsing them into `incompatible`
//!    would throw away the first one.
//!
//! Everything with a side effect is behind a trait, so a test can drive the whole
//! decision tree without spawning a process or touching a real database.

use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};
use yi_agent_lifecycle::deadline::Deadline;
use yi_agent_lifecycle::types::{ArtifactIdentity, RuntimeIdentity, RuntimeMode, RuntimeScope};

use super::selection::{RuntimeSelection, read_selection, write_selection};
use super::{CoordinationGuard, scope as scope_ops};

/// How long a start may take, end to end, by default.
///
/// One budget for the whole operation rather than per hop: the spec forbids budgets
/// that multiply, and a client that waited 30 seconds has waited 30 seconds whether
/// the time went to the lock, the spawn, or the handshake.
pub const STARTUP_BUDGET: Duration = Duration::from_secs(30);

/// How long an inspection may take per call.
///
/// Inspection runs on a UI poll, so it must fail fast and be free of side effects.
pub const INSPECT_BUDGET: Duration = Duration::from_secs(2);

/// Why a caller is bringing a runtime up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BringUpIntent {
    /// The caller will run work here.
    Business,
    /// The caller only needs a lease (observability, upgrade control).
    LeaseOnly,
    /// Read-only: report what is running and never start or register anything.
    Inspect,
}

impl BringUpIntent {
    fn may_start(self) -> bool {
        !matches!(self, Self::Inspect)
    }

    /// Inspection gets its own short ceiling regardless of the caller's budget.
    fn budget(self, deadline: Deadline) -> Deadline {
        if self == Self::Inspect {
            // `clamp` keeps the smaller of the two, so a caller that already
            // expects an answer in 100ms is not made to wait two seconds.
            Deadline::after(deadline.clamp(INSPECT_BUDGET))
        } else {
            deadline
        }
    }
}

/// The outcome of a bring-up attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BringUpResult {
    /// A managed runtime answered and this caller may use it.
    Connected {
        identity: RuntimeIdentity,
        control_path: PathBuf,
    },
    /// A runtime is running but is not the build this caller expected. Choosing
    /// between "use it anyway" and "switch it" is the user's call, never ours.
    VersionChoiceRequired { identity: RuntimeIdentity },
    /// Nothing automatic is safe. The reason is for display, not for parsing.
    ManualActionRequired { reason: String },
}

/// What a probe learned. Three outcomes, because two cannot express "asked and
/// did not get an answer", and that case must never become "start a new one".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProbeOutcome {
    Answered(RuntimeIdentity),
    /// Provably nothing is listening: connect was refused or the socket is gone.
    Absent,
    /// Something may be there, but did not answer usably -- a wedged listener, an
    /// exhausted descriptor table, a permission error, or a timeout.
    Unresponsive(String),
}

/// Reading the control channel. Injectable so no test needs a real guardian.
pub trait ControlProbe {
    fn probe(&self, control_path: &Path, deadline: &Deadline) -> ProbeOutcome;
}

/// What a read-only probe learned from the *business* socket.
///
/// A separate channel from the control one, because the two answer different
/// questions. Control says "is a supervised runtime here?"; business says "is
/// anything already serving work on this database?". The second one is what an
/// unsupervised `daemon serve` answers -- it has no guardian and therefore no
/// control channel at all -- so asking only control reads a live daemon as absent
/// and starts a second one on top of it.
///
/// Deliberately carries no identity: a business `Status` has no generation, so
/// turning one into a [`RuntimeIdentity`] would invent a claim the daemon never
/// made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BusinessProbeOutcome {
    /// Provably nothing is listening: the socket is gone or the connect was refused.
    Absent,
    /// Something answered a well-formed `Status`.
    Answered,
    /// Something may be there but did not answer usably. Never read as absence.
    Unresponsive(String),
}

/// Reading the business channel, read-only. Injectable so no test needs a socket.
pub trait BusinessProbe {
    fn probe(&self, scope: &RuntimeScope, deadline: &Deadline) -> BusinessProbeOutcome;
}

/// The real business channel: one bounded `Status` query.
pub struct BusinessChannelProbe;

impl BusinessProbe for BusinessChannelProbe {
    fn probe(&self, scope: &RuntimeScope, deadline: &Deadline) -> BusinessProbeOutcome {
        // Derived from the scope like the control path, not from the environment: a
        // probe that resolves the socket differently from the daemon reports a
        // missing file for a daemon that is right there.
        let socket =
            yi_agent_lifecycle::scope::daemon_socket(&scope.runtime_dir, &scope.instance_home);
        match yi_agent_store::ipc::send_status_until(&socket, deadline) {
            Ok(yi_agent_store::ipc::IpcResponse::Status { .. }) => BusinessProbeOutcome::Answered,
            // An answer of the wrong shape is still something being there. It must not
            // become "absent", because that is the value that starts a daemon.
            Ok(_) => BusinessProbeOutcome::Unresponsive(
                "the business socket answered something other than Status".to_string(),
            ),
            Err(error) => classify_business_error(error),
        }
    }
}

/// Only "nothing is listening" may read as absent.
///
/// `ENOENT` (no socket file) and `ECONNREFUSED` (a file, nobody accepting) are
/// proofs of absence. Everything else -- a timeout, a permission error, a truncated
/// frame, an identity mismatch -- means something *may* be there, and is reported as
/// such so the caller asks instead of launching.
fn classify_business_error(error: yi_agent_store::ipc::IpcError) -> BusinessProbeOutcome {
    let yi_agent_store::ipc::IpcError::Io(io) = &error else {
        return BusinessProbeOutcome::Unresponsive(error.to_string());
    };
    match io.raw_os_error() {
        Some(libc::ENOENT) | Some(libc::ECONNREFUSED) => BusinessProbeOutcome::Absent,
        _ => BusinessProbeOutcome::Unresponsive(error.to_string()),
    }
}

/// Starting a guardian, which in turn starts the one daemon.
///
/// A trait rather than a function pointer because a launcher carries configuration
/// (the cached artifact, the environment) that a bare `fn` cannot.
pub trait GuardianLauncher {
    fn launch(&self, scope: &RuntimeScope, expected: &ArtifactIdentity) -> io::Result<()>;
}

/// How compatible a running runtime is with the caller's expectation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compatibility {
    /// The exact artifact this caller expected.
    SameBuild,
    /// A different build, but one that can serve this caller's business protocol.
    CompatibleMismatch,
    /// Control works; business does not. Usable for a controlled switch, not for
    /// running tasks.
    ControlOnly,
    /// An old runtime that may still serve tasks but has no lifecycle guarantees.
    /// Usable only if the user explicitly accepts the exception.
    LegacyTemporaryChoiceRequired,
    /// Not enough verified information to do anything. Never business.
    Unknown,
}

/// Classifies a running identity against what this caller expected.
///
/// The order of the checks is the substance: a legacy process is *never* "same
/// build" even if its bytes happen to match, because what it lacks is the lifecycle
/// contract, not the code. And an unverifiable build is never upgraded to a
/// positive answer by a protocol match.
pub fn classify_compatibility(
    identity: &RuntimeIdentity,
    expected: &ArtifactIdentity,
    required_capabilities: &[String],
) -> Compatibility {
    if identity.mode == RuntimeMode::Unknown || identity.daemon_build_id.is_none() {
        return Compatibility::Unknown;
    }
    // A schema this build cannot read is not a compatibility question: writing to it
    // would corrupt data a newer build owns.
    if identity.schema_max < expected.schema_min || identity.schema_min > expected.schema_max {
        return Compatibility::Unknown;
    }
    if matches!(
        identity.mode,
        RuntimeMode::LegacyIndependent | RuntimeMode::LegacyEmbedded
    ) {
        // Old builds can serve the current business protocol, but they do not
        // implement leases, selection, or maintenance barriers. Reporting
        // "compatible" would promise an exit guarantee they cannot keep.
        return Compatibility::LegacyTemporaryChoiceRequired;
    }

    let business_usable = identity
        .business_protocol
        .is_some_and(|protocol| protocol >= expected.business_protocol_min)
        && identity
            .business_protocol
            .is_some_and(|protocol| protocol <= expected.business_protocol_max);
    let has_capabilities = required_capabilities
        .iter()
        .all(|required| identity.capabilities.iter().any(|have| have == required));
    let same_daemon = identity.daemon_build_id.as_deref() == Some(expected.build_id.as_str());
    let same_guardian = identity.guardian_build_id.as_deref() == Some(expected.build_id.as_str());

    if same_daemon && same_guardian && business_usable && has_capabilities {
        return Compatibility::SameBuild;
    }
    if business_usable && has_capabilities {
        // Same protocol, different bytes: business works, but "which build am I
        // talking to" is exactly the question the user must answer before an
        // upgrade.
        return Compatibility::CompatibleMismatch;
    }
    if identity
        .capabilities
        .iter()
        .any(|capability| capability == "control.v1")
    {
        return Compatibility::ControlOnly;
    }
    Compatibility::Unknown
}

/// Turns a probe outcome plus a classification into the caller's answer.
pub fn interpret(
    identity: RuntimeIdentity,
    control_path: &Path,
    expected: &ArtifactIdentity,
    required_capabilities: &[String],
    intent: BringUpIntent,
) -> BringUpResult {
    match classify_compatibility(&identity, expected, required_capabilities) {
        Compatibility::SameBuild => {
            if intent == BringUpIntent::Inspect || intent == BringUpIntent::LeaseOnly {
                return BringUpResult::Connected {
                    identity,
                    control_path: control_path.to_path_buf(),
                };
            }
            BringUpResult::Connected {
                identity,
                control_path: control_path.to_path_buf(),
            }
        }
        // Every non-exact answer needs a human decision. Business never proceeds on
        // a guess, and even a lease-only caller must not silently attach to a build
        // whose lifecycle it cannot rely on.
        Compatibility::CompatibleMismatch
        | Compatibility::ControlOnly
        | Compatibility::LegacyTemporaryChoiceRequired
        | Compatibility::Unknown => BringUpResult::VersionChoiceRequired { identity },
    }
}

/// The control socket for a scope.
///
/// Delegates to `yi-agent-lifecycle` rather than joining the file name again: the
/// lifecycle crate owns the `sun_path` fallback, and a second copy of that rule here
/// would eventually disagree with the one the guardian binds.
pub fn control_path(scope: &RuntimeScope) -> PathBuf {
    // Derived from the scope, never from the environment. The guardian binds from the
    // `YI_AGENT_INSTANCE_HOME` this process handed it, so a launcher that resolved the
    // path some other way would probe a socket nobody binds -- and report the failure
    // as a 30-second timeout instead of a path.
    yi_agent_lifecycle::scope::guardian_socket(&scope.runtime_dir, &scope.instance_home)
}

/// Brings a runtime up (or reports why it cannot) using the real process,
/// control-channel, and filesystem implementations.
pub fn ensure_managed(
    scope: &RuntimeScope,
    expected: &ArtifactIdentity,
    intent: BringUpIntent,
    deadline: Deadline,
) -> io::Result<BringUpResult> {
    let launcher = ProcessGuardian {
        instance_home: scope.instance_home.clone(),
    };
    ensure_managed_with(
        scope,
        expected,
        intent,
        deadline,
        &ControlChannelProbe,
        &launcher,
        &[],
    )
}

/// [`ensure_managed`] with every side effect injected.
///
/// `required_capabilities` is empty for a plain caller: it exists so a future
/// feature can say "I need `switch.preview.v1`" without inventing a second
/// compatibility function.
pub fn ensure_managed_with(
    scope: &RuntimeScope,
    expected: &ArtifactIdentity,
    intent: BringUpIntent,
    deadline: Deadline,
    probe: &dyn ControlProbe,
    launcher: &dyn GuardianLauncher,
    required_capabilities: &[String],
) -> io::Result<BringUpResult> {
    // Forwards, and deliberately does not repeat the decision: production callers go
    // through the business channel, and a second copy of the branch here is a second
    // place for the check to be forgotten.
    ensure_managed_with_business(
        scope,
        expected,
        intent,
        deadline,
        probe,
        &BusinessChannelProbe,
        launcher,
        required_capabilities,
    )
}

/// [`ensure_managed_with`] with the business channel injected.
///
/// The business pre-check is separate from the control probe because the two answer
/// different questions: control reports a *supervised* runtime, business reports
/// anything serving this database. An unsupervised daemon answers the second and not
/// the first, and starting beside it would put two writers on one database.
// Eight arguments, all of them injected dependencies of one decision: two probes, a
// launcher, the scope, the expectation, the intent, the budget and the capability
// list. Bundling them into a struct would move the same eight fields one line up and
// make the call sites read worse, which is the opposite of what the lint is for.
#[allow(clippy::too_many_arguments)]
pub fn ensure_managed_with_business(
    scope: &RuntimeScope,
    expected: &ArtifactIdentity,
    intent: BringUpIntent,
    deadline: Deadline,
    probe: &dyn ControlProbe,
    business: &dyn BusinessProbe,
    launcher: &dyn GuardianLauncher,
    required_capabilities: &[String],
) -> io::Result<BringUpResult> {
    let path = control_path(scope);

    // Inspection stops here when nothing answers. It must not take the lock: taking
    // it creates the runtime directory, and "asking what is running" creating a
    // runtime is the side effect this intent exists to avoid.
    if !intent.may_start() {
        let deadline = intent.budget(deadline);
        return Ok(match probe.probe(&path, &deadline) {
            ProbeOutcome::Answered(identity) => {
                interpret(identity, &path, expected, required_capabilities, intent)
            }
            // Control is silent, so nothing supervised is here -- but something may
            // still be serving the database, an unsupervised `daemon serve` the user
            // started. Reporting that as "nothing is running" is the answer a user
            // acts on by starting one, so the business channel is asked too. Still
            // read-only, and still before the coordination lock: it creates nothing.
            ProbeOutcome::Absent => match business.probe(scope, &deadline) {
                BusinessProbeOutcome::Absent => BringUpResult::ManualActionRequired {
                    reason: "no managed runtime is running, and inspection never starts one"
                        .to_string(),
                },
                BusinessProbeOutcome::Answered => BringUpResult::ManualActionRequired {
                    reason: "a daemon is serving this runtime without a supervisor; \
                             inspection will not replace it"
                        .to_string(),
                },
                BusinessProbeOutcome::Unresponsive(reason) => BringUpResult::ManualActionRequired {
                    reason: format!(
                        "the business socket did not answer ({reason}); refusing to guess"
                    ),
                },
            },
            ProbeOutcome::Unresponsive(reason) => BringUpResult::ManualActionRequired {
                reason: format!("the runtime did not answer ({reason}); refusing to guess"),
            },
        });
    }

    // One coordinator per scope decides at a time. Concurrent attaches queue here
    // and then observe the same daemon, instead of each starting one under a
    // project alias.
    // Business/lease callers get the full budget; the inspection ceiling only
    // applies to reads, never to a start that may legitimately take 30 seconds.
    let guard = CoordinationGuard::acquire(scope, deadline.clone())?;
    ensure_managed_under_guard_with_business(
        &HeldScope {
            guard,
            deadline,
            retirement_gate: None,
        },
        expected,
        intent,
        probe,
        business,
        launcher,
        required_capabilities,
    )
}

/// A runtime whose coordination lock the caller already holds.
///
/// The scope is reached *through* the guard instead of being a sibling field, so the
/// two cannot name different scopes -- a mistake the earlier shape allowed. It also
/// owns the guard, because the case this type exists for is a migration: that one
/// stops holding the lock after the managed runtime is connected, and it releases the
/// lock by dropping this value rather than by remembering to.
pub struct HeldScope {
    pub guard: CoordinationGuard,
    pub deadline: Deadline,
    /// The instance-wide retirement gate, when this caller took one.
    ///
    /// A migration must hold it until its freeze is *durable*: releasing it as soon
    /// as the scope lock is taken reopens exactly the window the gate exists to
    /// close -- a cooperating start could acquire the gate and begin an embedded
    /// daemon while the freeze marker is not yet on disk. `None` is the ordinary
    /// case (a plain managed start, or a caller whose own work already held it).
    pub retirement_gate: Option<super::migration_operation::RetirementGate>,
}

/// [`ensure_managed_with`] for a caller that already owns this scope's guard.
///
/// The guard is the reason this entry point exists: a migration holds the instance
/// gate *and* the scope guards, and re-acquiring a scope lock it already holds would
/// block on itself until the deadline. Passing the guard in makes that impossible by
/// construction instead of by a rule somebody has to remember.
pub fn ensure_managed_under_guard(
    held: &HeldScope,
    expected: &ArtifactIdentity,
    intent: BringUpIntent,
    probe: &dyn ControlProbe,
    launcher: &dyn GuardianLauncher,
    required_capabilities: &[String],
) -> io::Result<BringUpResult> {
    // Forwarder only: the decision lives in one place, the business-aware entry.
    ensure_managed_under_guard_with_business(
        held,
        expected,
        intent,
        probe,
        &BusinessChannelProbe,
        launcher,
        required_capabilities,
    )
}

/// [`ensure_managed_under_guard`] with the business channel injected.
///
/// The business check runs *inside* the coordination lock and before anything is
/// recorded or launched. Inside the lock because two attaches must not each decide
/// from a snapshot taken before the peer acted; before any write because a refusal
/// must leave no selection file, no database and no process behind.
pub fn ensure_managed_under_guard_with_business(
    held: &HeldScope,
    expected: &ArtifactIdentity,
    intent: BringUpIntent,
    probe: &dyn ControlProbe,
    business: &dyn BusinessProbe,
    launcher: &dyn GuardianLauncher,
    required_capabilities: &[String],
) -> io::Result<BringUpResult> {
    let guard = &held.guard;
    let deadline = held.deadline.clone();
    let scope = guard.scope();
    let path = control_path(scope);
    // A guard for another scope would serialise the wrong runtime and then check the
    // wrong claim, so it is refused here rather than producing a confusing answer.
    if guard.scope().key != scope.key || guard.scope().runtime_dir != scope.runtime_dir {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "the coordination guard belongs to {}, not {}",
                guard.scope().runtime_dir.display(),
                scope.runtime_dir.display()
            ),
        ));
    }
    // Re-check inside the lock: the peer that held it may have just started the
    // runtime we were about to start.
    //
    // An answer that is merely late is waited out here rather than failing at once.
    // The window being covered is real: a guardian that has just been launched has a
    // listening socket before it has finished reading the request, and reporting
    // that as "wedged" would turn every successful start into an intermittent
    // failure. What must never happen is treating it as absence -- so the wait ends
    // at the caller's budget, with a refusal, and never with a second launch.
    let mut first = probe.probe(&path, &deadline);
    while let ProbeOutcome::Unresponsive(reason) = &first {
        if deadline.is_expired() {
            // Something is there and is not answering. Starting a second daemon
            // would either fail on the instance lock or, worse, replace a live one.
            return Ok(BringUpResult::ManualActionRequired {
                reason: format!("the runtime did not answer ({reason}); refusing to guess"),
            });
        }
        std::thread::sleep(deadline.clamp(Duration::from_millis(20)));
        first = probe.probe(&path, &deadline);
    }
    match first {
        ProbeOutcome::Answered(identity) => {
            return Ok(interpret(
                identity,
                &path,
                expected,
                required_capabilities,
                intent,
            ));
        }
        // Handled by the loop above.
        ProbeOutcome::Unresponsive(_) => unreachable!("the wait loop drains this case"),
        ProbeOutcome::Absent => {}
    }

    // Nothing supervised answered, but something may still be serving this database:
    // an unsupervised `daemon serve` from an older build, or one the user started by
    // hand. Control cannot see it, so the business socket is asked directly. A
    // refusal here is the whole point of this check -- launching would put a second
    // daemon on the same database and the loser would be whichever one the user was
    // already using.
    match business.probe(scope, &deadline) {
        BusinessProbeOutcome::Absent => {}
        BusinessProbeOutcome::Answered => {
            return Ok(BringUpResult::ManualActionRequired {
                reason: "a daemon is already serving this runtime without a supervisor; \
                         stop it before starting a managed one"
                    .to_string(),
            });
        }
        BusinessProbeOutcome::Unresponsive(reason) => {
            return Ok(BringUpResult::ManualActionRequired {
                reason: format!(
                    "the business socket did not answer ({reason}); refusing to start a \
                     second daemon beside it"
                ),
            });
        }
    }

    // The directory may belong to another project or configuration instance. That
    // cannot be repaired by starting a daemon here: it would serve one database to
    // two scopes, so it is reported and the check happens while the lock is held.
    match claim_is_ours(guard, scope) {
        Ok(()) => {}
        Err(reason) => return Ok(BringUpResult::ManualActionRequired { reason }),
    }

    // The database must be readable by this build *before* anything writes to it.
    // A daemon that opened first would run migrations against a schema it does not
    // understand.
    let database = scope.runtime_dir.join("runtime.sqlite");
    if let Err(reason) = schema_is_readable(&database, expected) {
        return Ok(BringUpResult::ManualActionRequired { reason });
    }

    // Record what this runtime is supposed to run before starting it, so a crash
    // between launch and handshake leaves an explainable selection rather than
    // silence.
    record_selection(guard, expected)?;

    if let Err(error) = launcher.launch(scope, expected) {
        return Ok(BringUpResult::ManualActionRequired {
            reason: format!("could not start the managed runtime: {error}"),
        });
    }

    // The guardian binds its control socket before it starts the daemon, so waiting
    // for an answer is waiting for the whole chain, not just for `fork` to return.
    loop {
        match probe.probe(&path, &deadline) {
            ProbeOutcome::Answered(identity) => {
                return Ok(interpret(
                    identity,
                    &path,
                    expected,
                    required_capabilities,
                    intent,
                ));
            }
            ProbeOutcome::Absent | ProbeOutcome::Unresponsive(_) => {}
        }
        if deadline.is_expired() {
            return Ok(BringUpResult::ManualActionRequired {
                reason: format!(
                    "the managed runtime did not become ready within {}s",
                    STARTUP_BUDGET.as_secs()
                ),
            });
        }
        std::thread::sleep(deadline.clamp(Duration::from_millis(20)));
    }
}

/// Confirms — and on a first start, records — which scope owns this directory.
///
/// The coordination lock is what makes "first start" meaningful: without it two
/// coordinators could each read no claim and each write their own.
fn claim_is_ours(guard: &CoordinationGuard, scope: &RuntimeScope) -> Result<(), String> {
    use super::claim::{ScopeClaim, read_claim, write_claim};
    let existing = match read_claim(scope) {
        Ok(existing) => existing,
        Err(error) => {
            return Err(format!(
                "could not read the runtime's scope claim at {}: {error}",
                scope.runtime_dir.join(super::claim::CLAIM_FILE).display()
            ));
        }
    };
    let Some(claim) = existing else {
        return write_claim(guard, &ScopeClaim::of(scope)).map_err(|error| {
            format!(
                "could not record which project owns {}: {error}",
                scope.runtime_dir.display()
            )
        });
    };
    claim.conflict_with(scope).map_or(Ok(()), Err)
}

/// Whether this build can safely open (and migrate) the database.
///
/// Checked before launch because the daemon migrates the moment it opens. A missing
/// file is a first start and therefore fine.
/// Whether `expected` can open `database`, without writing to it.
///
/// Public so a version switch can do the same read-only check *before* it stops the
/// old runtime: discovering an unreadable schema after stopping would leave the user
/// with nothing running.
pub fn schema_is_readable_for(database: &Path, expected: &ArtifactIdentity) -> Result<(), String> {
    schema_is_readable(database, expected)
}

fn schema_is_readable(database: &Path, expected: &ArtifactIdentity) -> Result<(), String> {
    use yi_agent_store::schema_compat::inspect_schema;
    match inspect_schema(database) {
        Ok(None) => Ok(()),
        Ok(Some(version)) => {
            if version >= expected.schema_min && version <= expected.schema_max {
                Ok(())
            } else {
                Err(format!(
                    "the runtime database is schema {version}, outside this build's {}..{}; \
                     refusing to open it",
                    expected.schema_min, expected.schema_max
                ))
            }
        }
        Err(error) => Err(format!(
            "could not read the runtime database at {}: {error}",
            database.display()
        )),
    }
}

fn record_selection(guard: &CoordinationGuard, expected: &ArtifactIdentity) -> io::Result<()> {
    // Keep the previous outcome: it is what explains a failed switch that this
    // caller did not initiate.
    let last_operation =
        read_selection(guard.scope())?.and_then(|existing| existing.last_operation);
    write_selection(
        guard,
        &RuntimeSelection {
            target: expected.clone(),
            last_operation,
        },
    )
}

/// The real control channel.
pub struct ControlChannelProbe;

impl ControlProbe for ControlChannelProbe {
    fn probe(&self, control_path: &Path, deadline: &Deadline) -> ProbeOutcome {
        use yi_agent_lifecycle::client::ControlClient;
        use yi_agent_lifecycle::types::{ControlRequest, ControlResponse};

        if !control_path.exists() {
            return ProbeOutcome::Absent;
        }
        let mut client = match ControlClient::connect(control_path, deadline) {
            Ok(client) => client,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return ProbeOutcome::Absent,
            // Refused means the listener is gone; anything else means we were not
            // allowed to find out.
            Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
                return ProbeOutcome::Absent;
            }
            Err(error) => return ProbeOutcome::Unresponsive(error.to_string()),
        };
        match client.request(&ControlRequest::Identity, deadline) {
            Ok(ControlResponse::Identity(identity)) => ProbeOutcome::Answered(identity),
            Ok(other) => {
                ProbeOutcome::Unresponsive(format!("answered something unexpected: {other:?}"))
            }
            Err(error) => ProbeOutcome::Unresponsive(error.to_string()),
        }
    }
}

/// Starts a guardian from the pinned artifact.
pub struct ProcessGuardian {
    pub instance_home: PathBuf,
}

impl GuardianLauncher for ProcessGuardian {
    fn launch(&self, scope: &RuntimeScope, expected: &ArtifactIdentity) -> io::Result<()> {
        let (mut command, parent_end, child_end) = guardian_command(self, scope, expected)?;
        // The guardian must outlive the client that started it: an App that quits
        // must not take down the daemon a TUI still uses.
        let child_fd = {
            use std::os::fd::AsRawFd;
            child_end.as_raw_fd()
        };
        unsafe {
            use std::os::unix::process::CommandExt;
            command.pre_exec(move || {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                // `dup2` into the same number is a no-op and *keeps* CLOEXEC, which
                // would close the lifeline exactly at `exec`: the child would then
                // refuse to run with "no descriptor" for a reason nobody could see.
                if libc::dup2(child_fd, BOOTSTRAP_CHILD_FD) < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::fcntl(BOOTSTRAP_CHILD_FD, libc::F_SETFD, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn()?;
        // Our copy of the child's end must go now. Holding it would make that end
        // look alive forever, so the guardian would never see its lifeline fire.
        drop(child_end);
        // The parent's end is handed to a detached thread that holds it until this
        // process exits. Keeping it in a local would close it on return and hand the
        // guardian an immediate EOF -- a guardian that exits the moment it finished
        // starting, the exact opposite of outliving its client.
        //
        // The `Child` goes with it so that thread can also *reap* it. Dropping the
        // handle here is what produced `<defunct>` guardians: `std::process::Child`
        // does not wait on drop, and the desktop's app-server lives for hours, so
        // every guardian it started and later had killed stayed a zombie for as long
        // as the app ran. `kill -0` succeeds on a zombie, which made
        // `scripts/rebuild.sh stop` report "still running" and abort even though the
        // process was long dead (measured 2026-11-09).
        hold_lifeline(parent_end, child);
        Ok(())
    }
}

/// Builds the guardian launch command and both ends of its bootstrap lifeline.
///
/// Split out of [`ProcessGuardian::launch`] so the *declared identity* can be
/// asserted without spawning: the environment it sets is the whole contract between
/// launcher and guardian, and "which project did you start me for" must not be a
/// value the guardian is expected to infer.
pub fn guardian_command(
    guardian: &ProcessGuardian,
    scope: &RuntimeScope,
    expected: &ArtifactIdentity,
) -> io::Result<(
    std::process::Command,
    std::os::unix::net::UnixStream,
    std::os::unix::net::UnixStream,
)> {
    let program = pinned_program(&guardian.instance_home, expected)?;
    // The guardian refuses to run without an inherited descriptor, and that
    // refusal is the whole security boundary: "my parent started me as a managed
    // role" must be unfalsifiable by the child. A launcher that only set the
    // variable would hand the guardian a number that is not open (which aborts it
    // on `from_raw_fd`) or teach it to accept a hand-started process.
    //
    // A socketpair rather than a pipe because the descriptor is a *lifeline*, not
    // a message: the parent keeps its end open for exactly as long as it lives,
    // so EOF means "whoever assigned me this role is gone". A single byte could
    // express "started" but never "still here".
    let (parent_end, child_end) = std::os::unix::net::UnixStream::pair()?;
    let mut command = std::process::Command::new(&program);
    command
        .args(["internal-runtime", "guardian"])
        .current_dir(&scope.runtime_dir)
        .env("YI_AGENT_SCOPE_KEY", &scope.key)
        // The project is named explicitly. The guardian runs inside the runtime
        // directory, so `current_dir()` is the runtime -- deriving the project from
        // it would attribute this scope to whatever project owns that directory,
        // which is a different project whenever the runtime directory is shared or
        // overridden.
        .env(super::roles::PROJECT_KEY_ENV, scope.project_key.as_str())
        .env("YI_AGENT_INSTANCE_HOME", &scope.instance_home)
        // The number, not the descriptor: `pre_exec` installs it on a fixed
        // number, because a descriptor inherited at an arbitrary number could
        // collide with one the child opens before reading the variable.
        .env(
            super::roles::BOOTSTRAP_FD_ENV,
            BOOTSTRAP_CHILD_FD.to_string(),
        )
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    Ok((command, parent_end, child_end))
}

/// Whether two spellings of a runtime scope name the same directory.
///
/// The scope key is a path, and a path has more than one spelling: `/var` and
/// `/private/var` are one directory on macOS, and the launcher and the guardian can
/// hold different ones depending on which was canonicalised first. Comparing the raw
/// strings would call two identical scopes different -- and the guarded code reads
/// that disagreement as "someone swapped the scope", refusing a launch that is in
/// fact correct. Canonicalising both sides is what makes the comparison about the
/// directory rather than about the spelling.
pub fn scope_keys_agree(left: &str, right: &str) -> bool {
    if left == right {
        return true;
    }
    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        // An uncanonicalisable scope cannot be shown to agree, and guessing "yes"
        // would let two genuinely different scopes pass the check.
        _ => false,
    }
}

/// The number the guardian's bootstrap descriptor is installed on.
///
/// Above the standard streams and away from [`super::guardian::READY_FD`]: the two
/// descriptors carry different protocols, and sharing a number would make one read
/// the other's bytes.
const BOOTSTRAP_CHILD_FD: i32 = super::guardian::BOOTSTRAP_FD;

/// Keeps the launcher's end of the bootstrap lifeline open for this process's life.
///
/// A thread blocked reading the peer is the mechanism: the descriptor stays open
/// until the process exits, and a socketpair only produces EOF when the *other* end
/// closes. That makes "the launcher is gone" observable without inventing an extra
/// protocol. Detached on purpose: there is nothing to join, and joining it would
/// block the caller until the guardian exits.
fn hold_lifeline(parent_end: std::os::unix::net::UnixStream, guardian: std::process::Child) {
    let guardian_pid = guardian.id();
    // Reap the guardian, or it stays `<defunct>` for as long as this process lives.
    // A separate thread because `wait` blocks as long as the guardian runs, while the
    // lifeline below blocks on a descriptor that only closes when *we* exit.
    std::thread::Builder::new()
        .name(format!("guardian-reaper-{guardian_pid}"))
        .spawn(move || {
            let mut guardian = guardian;
            let _ = guardian.wait();
        })
        .ok();
    std::thread::Builder::new()
        .name(format!("guardian-lifeline-{guardian_pid}"))
        .spawn(move || {
            let mut sink = [0u8; 64];
            let mut stream = parent_end;
            // No frame is ever sent in either direction: this descriptor proves
            // liveness, and a payload would quietly turn it into a command channel.
            while let Ok(read) = stream.read(&mut sink) {
                if read == 0 {
                    break;
                }
            }
        })
        .ok();
}

/// Where a pinned copy of one exact artifact lives.
///
/// A build id is embedded before packaging signs the executable. CLI and desktop
/// signatures can therefore have the same build id but different approved bytes.
/// Keeping the digest in the path lets both stay pinned without overwriting a
/// committed artifact. Old build-only entries remain untouched; new callers stage
/// their verified source into this layout instead of trusting or repairing them.
pub fn cached_artifact_path(instance_home: &Path, build_id: &str, digest: &str) -> PathBuf {
    instance_home
        .join("artifacts")
        .join(build_id)
        .join(digest)
        .join("yi-agent")
}

/// A verified private copy of the artifact plus the identity that describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinnedArtifact {
    pub identity: ArtifactIdentity,
    pub path: PathBuf,
}

/// Pins `source` into the instance's private directory and returns the copy.
///
/// The copy exists because a bundle can be replaced or deleted while a runtime is
/// still supposed to run *that* build. Pointing the launcher at the bundle's path
/// would let an in-place upgrade silently change which bytes an already-committed
/// selection refers to.
///
/// `sha256` on the returned identity is the digest of the copy, computed here: a
/// caller that supplied an empty digest gets a verified one, and a caller that
/// supplied a different one gets an error rather than a half-trusted file.
pub fn pin_artifact(instance_home: &Path, source: &ArtifactIdentity) -> io::Result<PinnedArtifact> {
    let directory = instance_home.join("artifacts").join(&source.build_id);
    std::fs::create_dir_all(&directory)?;
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
    // Exclusive creation and a per-process sequence keep concurrent pins from
    // truncating one another. Retry stale names left by a previous process id.
    static NEXT_TEMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut input = std::fs::File::open(&source.executable)?;
    let (temp, mut output) = loop {
        let sequence = NEXT_TEMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let temp = directory.join(format!(".yi-agent.{}.{sequence}.tmp", std::process::id()));
        match std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o700)
            .open(&temp)
        {
            Ok(output) => break (temp, output),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    };

    let digest = {
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = input.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            output.write_all(&buffer[..read])?;
        }
        output.sync_all()?;
        hex(&hasher.finalize())
    };
    if !source.sha256.is_empty() && source.sha256 != digest {
        let _ = std::fs::remove_file(&temp);
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "the artifact at {} changed while it was being pinned (expected digest {})",
                source.executable.display(),
                source.sha256
            ),
        ));
    }
    let destination = cached_artifact_path(instance_home, &source.build_id, &digest);
    let digest_directory = destination.parent().expect("an artifact has a parent");
    std::fs::create_dir_all(digest_directory)?;
    std::fs::set_permissions(digest_directory, std::fs::Permissions::from_mode(0o700))?;
    std::fs::rename(&temp, &destination)?;
    std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o700))?;

    let mut identity = source.clone();
    identity.sha256 = digest;
    identity.executable = destination.clone();
    Ok(PinnedArtifact {
        identity,
        path: destination,
    })
}

/// Returns the pinned copy, verifying it still has the digest the selection
/// recorded.
///
/// A missing or changed copy is an error. Falling back to the bundle or to `PATH`
/// would run bytes the selection never approved.
pub fn verify_pinned(instance_home: &Path, expected: &ArtifactIdentity) -> io::Result<PathBuf> {
    let path = cached_artifact_path(instance_home, &expected.build_id, &expected.sha256);
    let digest = digest_of(&path)?;
    if expected.sha256.is_empty() || digest != expected.sha256 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "the pinned artifact at {} does not match its recorded digest; \
                 re-pin it before starting",
                path.display()
            ),
        ));
    }
    Ok(path)
}

/// The program to launch: the pinned copy when the selection names one, otherwise
/// the caller's own executable.
///
/// A selection that carries a digest names bytes that must be *pinned* before they
/// can be launched, and nothing else in the startup path pins them: `record_selection`
/// only writes the intent, and the launcher runs strictly after it. So a first start
/// for a build has no copy yet, and [`verify_pinned`] would answer `NotFound` for a
/// directory that was never created. That is not "the bytes changed", it is "the
/// bytes were never staged", and the two must not be conflated: refusing here made
/// every fresh install unable to start its managed runtime at all.
///
/// Pinning is not a fallback to the bundle. [`pin_artifact`] reads
/// `expected.executable`, refuses to finish unless the digest it computed equals
/// `expected.sha256`, and only then `rename`s the copy into place. The bytes that
/// end up launched are therefore the bytes the selection approved -- exactly what
/// `verify_pinned` would have demanded. A copy that exists but disagrees with the
/// recorded digest is still an error and is left untouched: that case really is a
/// committed selection whose bytes changed, and silently overwriting it would hide
/// an in-place upgrade from the supervisor.
pub fn pinned_program(instance_home: &Path, expected: &ArtifactIdentity) -> io::Result<PathBuf> {
    if expected.sha256.is_empty() {
        return Ok(expected.executable.clone());
    }
    match verify_pinned(instance_home, expected) {
        Ok(path) => Ok(path),
        // Only `NotFound` means "not staged yet". `digest_of` turns a missing file
        // into exactly this kind, while a digest mismatch is `InvalidData`.
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            // Preserve a previously pinned selection even if its bundle is gone.
            // The old layout can also hold another approved signature of this
            // build, so only migrate it when its bytes match this exact selection.
            let legacy = instance_home
                .join("artifacts")
                .join(&expected.build_id)
                .join("yi-agent");
            let mut source = expected.clone();
            if digest_of(&legacy).is_ok_and(|digest| digest == expected.sha256) {
                source.executable = legacy;
            }
            // Recheck the digest while copying, including legacy migration.
            pin_artifact(instance_home, &source)?;
            verify_pinned(instance_home, expected)
        }
        Err(error) => Err(error),
    }
}

fn digest_of(path: &Path) -> io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Resolves the canonical scope for an existing runtime directory without creating
/// anything.
pub fn scope_for_existing(
    runtime_dir: &Path,
    project: &Path,
    instance_home: &Path,
) -> io::Result<RuntimeScope> {
    scope_ops::resolve(runtime_dir, project, instance_home)
}

#[cfg(test)]
mod reaper_tests {
    //! `hold_lifeline` 必须同时收尸。
    //!
    //! 回归缺陷（2026-11-09 实测）：`launch` 里 spawn 出的 `Child` 直接被丢弃。
    //! `std::process::Child` 在 `Drop` 里**不** `wait`，而桌面 app-server 会连续
    //! 运行数小时，于是每一个被它启动、随后被杀掉的 guardian 都停在 `<defunct>`。
    //! `kill -0` 对僵尸返回成功，`scripts/rebuild.sh stop` 因此报「仍有进程停不下来」
    //! 并中止——尽管进程早已退出。
    use super::hold_lifeline;

    /// 一个进程被 kill 之后，它的 pid 不再以 `Z` 留在进程表里。
    #[cfg(unix)]
    #[test]
    fn a_reaped_child_does_not_linger_as_a_zombie() {
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        // 用一个立刻退出的小程序充当 guardian：它不需要理解 lifeline，
        // 只要能被 spawn 并能被 reap。
        let child = Command::new("/bin/sh")
            .arg("-c")
            .arg("exit 0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("the fixture process starts");
        let pid = child.id();

        let (parent_end, _child_end) = std::os::unix::net::UnixStream::pair().expect("socketpair");
        hold_lifeline(parent_end, child);

        // 等 reaper 线程把它收掉。它退出后 `ps -o stat=` 要么查不到该 pid，
        // 要么显示 `Z`——后者正是本测试要排除的形态。
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut last = String::new();
        while Instant::now() < deadline {
            match std::process::Command::new("ps")
                .args(["-o", "stat=", "-p", &pid.to_string()])
                .output()
            {
                Ok(output) => {
                    last = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if last.is_empty() {
                        return;
                    }
                    if !last.starts_with('Z') && !last.is_empty() {
                        // 还没退出，再等。
                    }
                }
                Err(_) => return,
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !last.starts_with('Z'),
            "pid {pid} is still a zombie after 5s (stat {last:?}); \
             the reaper thread did not call wait()"
        );
    }
}
