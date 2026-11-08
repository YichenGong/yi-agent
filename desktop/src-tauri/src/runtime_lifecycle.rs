//! Runtime lifecycle ownership for the desktop shell.
//!
//! The shell — not the sidecar — is the App. An `app-server` restart must not
//! change the App's identity or drop its claim on a project's runtime, and a dead
//! sidecar must not be able to keep someone else's runtime alive. So the lease,
//! the heartbeat and the quit barrier live on the Tauri side, while the sidecar
//! receives only a narrow, non-renewable statement of who the local session
//! speaks for (see `yi_agent_app_server`'s shell-authorization plumbing).
//!
//! Two rules shape this module:
//!
//! * **One total budget per quit.** Cmd+Q is one user decision, so every project
//!   is released *in parallel* under one deadline. Sequential release would
//!   multiply the wait by the number of open projects, and the spec is explicit
//!   that the grace period is global, not per project.
//! * **Never block the event loop.** The control protocol's client is a blocking
//!   socket client. It runs on `spawn_blocking`, and every wait is additionally
//!   capped by `tokio::time::timeout(remaining)`: a wedged daemon can delay the
//!   quit only until the budget runs out, never indefinitely.

use std::collections::BTreeMap;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use yi_agent_lifecycle::deadline::{Clock, Deadline};
use yi_agent_lifecycle::foreground::RootBindIntent;
use yi_agent_lifecycle::production::{PrepareOutcome, PrepareRequest, RootIntentRequest};
use yi_agent_lifecycle::types::{
    ArtifactIdentity, ClientKind, LeaseMode, OperationId, OperationStatus, ReclaimResult,
    RestartPreview, RootAuthorization, RuntimeScope, RuntimeStatusSnapshot,
};

/// The global graceful budget for one quit, shared by every project.
///
/// The spec's teardown split (10 s graceful, then up to 5 s confirming a forced
/// stop) happens *inside* the guardian, per client. This is the shell's own bound
/// on how long it waits for those answers. Timing out is a real outcome: past the
/// budget the shell exits and lets the guardian finish its already-started,
/// isolated reclaim.
pub const QUIT_GRACE: Duration = Duration::from_secs(10);

/// Budget for one acquisition. Acquisition only registers a lease; the runtime's
/// own 30-second first-client window belongs to the daemon, not to this call.
const ACQUIRE_BUDGET: Duration = yi_agent_lifecycle::CONTROL_REQUEST_TIMEOUT;

/// Budget for starting the renewal thread.
///
/// Spawning a thread is local work, so this only has to cover opening the lease's
/// record and handing it to the thread -- not a network round trip. It exists at
/// all because the call crosses `spawn_blocking`, and an unbounded wait there would
/// let a poisoned lock hold the whole acquisition open.
const HEARTBEAT_START_BUDGET: Duration = Duration::from_secs(5);

/// Budget for one read-only request (status, preview, operation poll).
///
/// One control frame's ceiling, not a retry loop: a wedged guardian must make the
/// banner say "unknown" quickly, and the UI polls again anyway.
const STATUS_BUDGET: Duration = yi_agent_lifecycle::CONTROL_REQUEST_TIMEOUT;

/// Budget for a confirmation.
///
/// A confirmation is answered as soon as the switch is *accepted*, not when it
/// completes -- completion is what `runtime_operation` is for. The larger budget
/// covers taking the coordination lock and writing the atomic selection record,
/// both of which touch the filesystem.
pub const SWITCH_BUDGET: Duration = Duration::from_secs(10);

/// Budget for one legacy-migration step.
///
/// Larger than a control round trip on purpose: a preview probes the platform
/// (launchd, peer credentials, process starts), and confirm re-probes before it
/// touches anything. Both are local work with a fixed upper bound, not network.
pub const MIGRATION_BUDGET: Duration = Duration::from_secs(30);

/// The guardian's control socket file inside a runtime directory.
///
/// The definition is shared (`yi_agent_lifecycle::GUARDIAN_SOCKET_FILE`), not
/// duplicated: the shell must connect to the exact socket the guardian binds, and
/// `yi-agent-supervisors` cannot be linked here because it carries the store (and
/// therefore SQLite).
pub use yi_agent_lifecycle::GUARDIAN_SOCKET_FILE;

/// A monotonic clock that follows Tokio's clock.
///
/// In production this is indistinguishable from
/// [`yi_agent_lifecycle::deadline::MonotonicClock`]. The point is tests:
/// `#[tokio::test(start_paused = true)]` freezes `tokio::time`, so a deadline
/// built on this clock expires exactly when the test advances time. Pairing a
/// paused runtime with `std::time::Instant` would silently measure nothing.
#[derive(Clone, Copy, Debug)]
pub struct TokioClock {
    origin: tokio::time::Instant,
}

impl TokioClock {
    pub fn new() -> Self {
        Self {
            origin: tokio::time::Instant::now(),
        }
    }
}

impl Default for TokioClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for TokioClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }
}

/// The shell's control-channel surface, as the manager needs it.
///
/// A trait so the quit sequence can be tested against a fake that withholds
/// answers on command. The real implementation drives the blocking client; a
/// fake never touches a socket.
pub trait ControlBackend: Send + Sync + 'static {
    /// Register this shell's lease for one project, without starting the renewal.
    ///
    /// Registration and renewal are separate calls because they are separate
    /// facts, and the order between them is part of the contract: a heartbeat
    /// started before a registration succeeds would renew a grant that does not
    /// exist yet. Keeping them apart lets a caller (and a test) observe
    /// `register` -> `start_heartbeat` rather than infer it.
    ///
    /// Returns whether a lease is actually now held. `Ok(false)` is the honest
    /// answer when no guardian serves this scope (embedded or legacy runtime):
    /// there is then nothing the quit could release, and registering the project
    /// anyway would make every exit report a reclaim failure that never happened.
    fn register(
        &self,
        client_run_id: &str,
        scope: &RuntimeScope,
        mode: LeaseMode,
        budget: Duration,
    ) -> io::Result<bool>;

    /// Begin renewing the lease [`Self::register`] just took for `scope`.
    ///
    /// Fails when no registration is live: renewing nothing is not a success, and
    /// a lease that is never renewed looks held until the guardian expires it,
    /// which is exactly the window where the shell would believe it may work.
    fn start_heartbeat(&self, scope: &RuntimeScope) -> io::Result<()>;

    /// The mode of the lease currently held for `scope`, when one is held.
    ///
    /// `None` means "no lease, or it cannot be read". Readiness must check this
    /// rather than remember the mode it asked for: a guardian may answer a
    /// registration with a *queued* grant (`WaitingNextGeneration`) when a switch
    /// is in progress, and a queued client is the one thing that is certainly not
    /// ready.
    fn lease_mode(&self, scope: &RuntimeScope) -> Option<LeaseMode> {
        let _ = scope;
        None
    }

    /// Whether the lease for `scope` is still honoured by its guardian.
    ///
    /// Fail-closed by default: a backend that cannot answer is not evidence of a
    /// live lease, and readiness must never be inferred from silence.
    fn lease_is_live(&self, scope: &RuntimeScope) -> bool {
        let _ = scope;
        false
    }

    /// Ask the guardian for a non-renewable root authorization on `scope`.
    ///
    /// Only reachable through the native gate: a WebView must not be able to ask
    /// for a root, because the answer is the capability a tool process runs under.
    fn authorize_root(
        &self,
        scope: &RuntimeScope,
        intent: &RootBindIntent,
        budget: Duration,
    ) -> io::Result<RootAuthorization> {
        let _ = (scope, intent, budget);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this control backend cannot authorize a root",
        ))
    }

    /// Tells the guardian that somebody does (or no longer does) want `plugin`.
    ///
    /// Uses the lease this shell already holds: demand is owner-bound, and a
    /// "demand" that named its own owner would be a way to keep somebody else's
    /// service alive. Returns `(wanted_after, changed)`.
    ///
    /// Defaulted to `Unsupported`, because a backend with no lease has nobody to
    /// speak for -- that is the embedded/legacy runtime, where plugins are started
    /// by the old supervisor rather than by demand.
    fn note_plugin_demand(
        &self,
        scope: &RuntimeScope,
        plugin: &str,
        acquire: bool,
        budget: Duration,
    ) -> io::Result<(bool, bool)> {
        let _ = (scope, plugin, acquire, budget);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this control backend cannot carry plugin demand",
        ))
    }

    /// Release the project and wait for the verified teardown, bounded by
    /// `budget`. A result that did not observe a stop must say `stopped: false`.
    fn release(
        &self,
        client_run_id: &str,
        scope: &RuntimeScope,
        budget: Duration,
    ) -> io::Result<ReclaimResult>;

    /// One read-only status page. `Ok(None)` means nothing serves this scope.
    ///
    /// Defaulted to `Unsupported` rather than required: a fake that only models
    /// quitting must not be forced to invent a status, and an honest "I cannot
    /// answer" is better than a fake zero that a test could mistake for a healthy
    /// runtime.
    fn status(
        &self,
        scope: &RuntimeScope,
        budget: Duration,
    ) -> io::Result<Option<RuntimeStatusSnapshot>> {
        let _ = (scope, budget);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this control backend cannot report runtime status",
        ))
    }

    /// Read-only impact preview for replacing the runtime with `target`.
    fn preview(
        &self,
        client_run_id: &str,
        scope: &RuntimeScope,
        target: ArtifactIdentity,
        budget: Duration,
    ) -> io::Result<RestartPreview> {
        let _ = (client_run_id, scope, target, budget);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this control backend cannot preview a restart",
        ))
    }

    /// Consume a preview token, bound to `operation_id` chosen before the click.
    fn confirm(
        &self,
        client_run_id: &str,
        scope: &RuntimeScope,
        token: &str,
        operation_id: &str,
        budget: Duration,
    ) -> io::Result<OperationId> {
        let _ = (client_run_id, scope, token, operation_id, budget);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this control backend cannot confirm a restart",
        ))
    }

    /// The live lease this backend holds for `scope`.
    ///
    /// Defaulted to `NotFound` so a fake that only models quitting does not have to
    /// invent a lease. The real backend returns the one it registered, because
    /// recovery is owner-bound and the App must present its own grant.
    fn lease_for_scope(
        &self,
        scope: &RuntimeScope,
    ) -> io::Result<Arc<Mutex<yi_agent_lifecycle::client::FrontendLease>>> {
        let _ = scope;
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "this control backend holds no lease",
        ))
    }

    /// One read of an operation's progress. `Ok(None)` = this runtime never ran it.
    fn operation(
        &self,
        client_run_id: &str,
        scope: &RuntimeScope,
        operation_id: &str,
        budget: Duration,
    ) -> io::Result<Option<OperationStatus>> {
        let _ = (client_run_id, scope, operation_id, budget);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this control backend cannot report an operation",
        ))
    }
}

/// The real backend: one [`yi_agent_lifecycle::client::FrontendLease`] per
/// project, held here so the heartbeat outlives any single sidecar.
pub struct SocketBackend {
    /// One lease per project, each behind its own lock.
    ///
    /// Two locks rather than one: a preview or a confirm on a wedged project would
    /// otherwise hold the table while the socket call runs, and every other
    /// project's acquire (including the quit barrier's releases) would wait behind
    /// it. The inner lock still serialises the calls that belong to one project,
    /// which is the only serialisation those calls need.
    leases: Mutex<BTreeMap<String, Arc<Mutex<yi_agent_lifecycle::client::FrontendLease>>>>,
}

/// Locks one project's lease.
fn lock_lease(
    lease: &Mutex<yi_agent_lifecycle::client::FrontendLease>,
) -> io::Result<std::sync::MutexGuard<'_, yi_agent_lifecycle::client::FrontendLease>> {
    lease
        .lock()
        .map_err(|_| io::Error::other("the lifecycle lease is poisoned"))
}

impl SocketBackend {
    pub fn new() -> Self {
        Self {
            leases: Mutex::new(BTreeMap::new()),
        }
    }

    fn control_path(scope: &RuntimeScope) -> std::path::PathBuf {
        yi_agent_lifecycle::scope::control_path(&scope.runtime_dir)
    }
}

impl Default for SocketBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl ControlBackend for SocketBackend {
    fn register(
        &self,
        client_run_id: &str,
        scope: &RuntimeScope,
        mode: LeaseMode,
        budget: Duration,
    ) -> io::Result<bool> {
        let path = Self::control_path(scope);
        let mut leases = self.leases.lock().map_err(|_| poisoned())?;

        // A live lease is not re-registered on every window switch: registration
        // is the App's identity, and a second one would be a second client.
        if let Some(existing) = leases.get(&scope.key) {
            if !lock_lease(existing)?.is_lost() {
                return Ok(true);
            }
        }
        leases.remove(&scope.key);

        let deadline = Deadline::after(budget);
        let lease = match yi_agent_lifecycle::client::FrontendLease::register_if_served(
            &path,
            client_run_id.to_string(),
            ClientKind::App,
            mode,
            &deadline,
        )? {
            // Embedded or legacy runtime with no guardian: there is no lease to
            // hold, and inventing one would attribute this process's lifetime to
            // a runtime that does not manage it.
            None => return Ok(false),
            Some(lease) => lease,
        };
        leases.insert(scope.key.clone(), Arc::new(Mutex::new(lease)));
        Ok(true)
    }

    fn start_heartbeat(&self, scope: &RuntimeScope) -> io::Result<()> {
        let lease = self.lease_for(scope)?;
        let lease = lock_lease(&lease)?;
        lease.start_heartbeat()
    }

    fn lease_mode(&self, scope: &RuntimeScope) -> Option<LeaseMode> {
        let lease = self.lease_for(scope).ok()?;
        let lease = lock_lease(&lease).ok()?;
        Some(lease.grant().mode)
    }

    fn lease_is_live(&self, scope: &RuntimeScope) -> bool {
        match self.lease_for(scope) {
            Ok(lease) => lock_lease(&lease)
                .map(|lease| !lease.is_lost())
                .unwrap_or(false),
            Err(_) => false,
        }
    }

    fn authorize_root(
        &self,
        scope: &RuntimeScope,
        intent: &RootBindIntent,
        budget: Duration,
    ) -> io::Result<RootAuthorization> {
        let lease = self.lease_for(scope)?;
        let lease = lock_lease(&lease)?;
        lease.authorize_root(intent, &Deadline::after(budget))
    }

    fn note_plugin_demand(
        &self,
        scope: &RuntimeScope,
        plugin: &str,
        acquire: bool,
        budget: Duration,
    ) -> io::Result<(bool, bool)> {
        let lease = self.lease_for(scope)?;
        let lease = lock_lease(&lease)?;
        let deadline = Deadline::after(budget);
        if acquire {
            lease.acquire_plugin_demand(plugin, &deadline)
        } else {
            lease.release_plugin_demand(plugin, &deadline)
        }
    }

    fn lease_for_scope(
        &self,
        scope: &RuntimeScope,
    ) -> io::Result<Arc<Mutex<yi_agent_lifecycle::client::FrontendLease>>> {
        self.lease_for(scope)
    }

    fn release(
        &self,
        client_run_id: &str,
        scope: &RuntimeScope,
        budget: Duration,
    ) -> io::Result<ReclaimResult> {
        let lease = {
            let mut leases = self.leases.lock().map_err(|_| poisoned())?;
            leases.remove(&scope.key)
        };
        let Some(lease) = lease else {
            // Nothing registered for this project: there is nothing to reclaim,
            // and claiming a stop nobody asked for would be a false success.
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no lease was held for {}", scope.key),
            ));
        };
        let deadline = Deadline::after(budget);
        let lease = lock_lease(&lease)?;
        let operation = lease.release(&deadline)?;
        match lease.await_result(&operation, budget)? {
            Some(result) => Ok(result),
            None => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("the reclaim for {client_run_id} was not confirmed within the budget"),
            )),
        }
    }

    fn status(
        &self,
        scope: &RuntimeScope,
        budget: Duration,
    ) -> io::Result<Option<RuntimeStatusSnapshot>> {
        // Deliberately does not consult `self.leases`: a status read must not
        // register, renew, or require a lease. That is also why the guardian
        // accepts this request without a grant.
        let path = Self::control_path(scope);
        let deadline = Deadline::after(budget);
        if !path.exists() {
            return Ok(None);
        }
        let mut client = match yi_agent_lifecycle::client::ControlClient::connect(&path, &deadline)
        {
            Ok(client) => client,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                return Ok(None)
            }
            Err(error) => return Err(error),
        };
        match client.request(
            &yi_agent_lifecycle::types::ControlRequest::Status { cursor: None },
            &deadline,
        )? {
            yi_agent_lifecycle::types::ControlResponse::Status(snapshot) => Ok(Some(*snapshot)),
            // An older guardian that has no status request: it is there, it just
            // cannot describe itself yet.
            yi_agent_lifecycle::types::ControlResponse::Unsupported { .. } => Ok(None),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("the runtime answered a status read with {other:?}"),
            )),
        }
    }

    fn preview(
        &self,
        client_run_id: &str,
        scope: &RuntimeScope,
        target: ArtifactIdentity,
        budget: Duration,
    ) -> io::Result<RestartPreview> {
        let _ = client_run_id;
        let lease = self.lease_for(scope)?;
        let lease = lock_lease(&lease)?;
        lease.preview(target, &Deadline::after(budget))
    }

    fn confirm(
        &self,
        client_run_id: &str,
        scope: &RuntimeScope,
        token: &str,
        operation_id: &str,
        budget: Duration,
    ) -> io::Result<OperationId> {
        let _ = client_run_id;
        let lease = self.lease_for(scope)?;
        let lease = lock_lease(&lease)?;
        lease.confirm(token, operation_id, &Deadline::after(budget))
    }

    fn operation(
        &self,
        client_run_id: &str,
        scope: &RuntimeScope,
        operation_id: &str,
        budget: Duration,
    ) -> io::Result<Option<OperationStatus>> {
        let _ = client_run_id;
        let lease = self.lease_for(scope)?;
        let lease = lock_lease(&lease)?;
        lease.operation(operation_id, &Deadline::after(budget))
    }
}

impl SocketBackend {
    /// The live lease for `scope`, or a refusal that says what to do about it.
    ///
    /// Preview and confirm are owner-bound at the runtime (the grant must match a
    /// live lease), so without our own lease there is nothing to present. The error
    /// says "acquire first" rather than "not found", because the fix is in the
    /// caller's hands.
    fn lease_for(
        &self,
        scope: &RuntimeScope,
    ) -> io::Result<Arc<Mutex<yi_agent_lifecycle::client::FrontendLease>>> {
        let leases = self.leases.lock().map_err(|_| poisoned())?;
        match leases.get(&scope.key) {
            Some(lease) if !lock_lease(lease)?.is_lost() => Ok(Arc::clone(lease)),
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "this app has no live lease on {}; open the project first",
                    scope.key
                ),
            )),
        }
    }
}

fn poisoned() -> io::Error {
    io::Error::other("the lifecycle lease table is poisoned")
}

// ---------------------------------------------------------------------------
// Project acquisition: what the user is told vs what the shell actually holds
// ---------------------------------------------------------------------------

/// Where a project's preparation stands, as the page is allowed to see it.
///
/// A judgement, not a boolean: `starting` is not `failed`, and `unknown` is not
/// `blocked`. Collapsing them into `Result<(), _>` is what once made "the runtime
/// is still starting" render as "your project is broken", and it is also what made
/// the page believe a lease existed because a call had returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AcquireState {
    /// A preparation for this scope is in flight. Nothing is held yet.
    Starting,
    /// The shell holds a live, renewing lease and the runtime identity matches
    /// this build. This is `lifecycleReady` only -- every session still needs its
    /// own `rootReady` before it may run.
    ManagedReady,
    /// An unmanaged runtime is serving this project. Only an explicit user choice
    /// may use it; the shell does not silently adopt it.
    LegacyChoiceRequired,
    /// A condition failed: incompatibility, identity mismatch, a refused lease.
    /// Never to be downgraded to an embedded runtime.
    Blocked,
    /// Something could not be verified, or the answer was lost. Read-only
    /// reconciliation -- not evidence of failure and not evidence of success.
    Unknown,
}

/// What a caller may do next, in terms the page can act on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RetryClass {
    /// Retrying later is safe: the attempt never started any runtime work.
    Backoff,
    /// Retrying now would duplicate a side effect that is already underway.
    DoNotRetry,
    /// Requires a user decision (version switch, legacy adoption, migration).
    UserAction,
    /// Nothing is wrong; wait for the in-flight attempt to finish.
    WhenIdle,
}

/// The only project-acquisition shape that crosses to the WebView.
///
/// Deliberately carries no grant, no capability and no daemon path: the page needs
/// to know *whether* it may expect a runtime, not what credential proves it. The
/// id names this attempt so a late answer cannot be mistaken for a newer one.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectAcquireView {
    pub scope_key: String,
    pub attempt_id: String,
    pub state: AcquireState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub retry: RetryClass,
}

/// What one project's preparation actually produced, kept apart from the view.
///
/// `held` and `ready` are two different facts and are stored as two different
/// fields on purpose:
///
/// * `held` is set the moment a lease is registered, and survives a later failure
///   (a root that cannot be authorized does not revoke the lease).
/// * `ready` is only set after the heartbeat is confirmed *and* the identity is
///   still current. Losing either clears it.
///
/// A `blocked` view with `held == true` is a normal state: a compatible build
/// mismatch may preview and confirm a switch using the lease it holds, while
/// business work stays shut until the switch completes.
#[derive(Clone, Debug)]
pub struct ProjectState {
    pub scope: RuntimeScope,
    /// The attempt whose result this state describes. A result from any other
    /// attempt is discarded rather than applied.
    pub attempt_id: String,
    pub view: ProjectAcquireView,
    pub held: bool,
    pub ready: bool,
}

/// One in-flight acquisition per scope, so concurrent callers share one attempt.
type AttemptSlot = tokio::sync::Mutex<()>;

/// Where the current local management session comes from.
///
/// A lookup rather than a stored handle: the session is created when the sidecar
/// spawns and replaced on every restart, while the manager is installed before the
/// first spawn. Handing the channel one session at construction would leave it
/// talking to a generation that has already exited.
pub trait ManagementSessions: Send + Sync + 'static {
    fn current(&self) -> Option<Arc<crate::runtime_management::ManagementSession>>;
    fn transport(&self) -> Arc<dyn crate::runtime_management::PrepareTransport>;
}

/// The preparation channel the packaged shell uses: the private stdio management
/// session of its own local sidecar.
pub struct LocalPrepareChannel {
    sessions: Arc<dyn ManagementSessions>,
    manager: std::sync::Weak<DesktopRuntimeManager>,
}

impl LocalPrepareChannel {
    pub fn new(sessions: Arc<dyn ManagementSessions>) -> Self {
        Self {
            sessions,
            manager: std::sync::Weak::new(),
        }
    }

    /// Points the channel at the manager whose epoch it should record.
    pub fn with_manager(mut self, manager: std::sync::Weak<DesktopRuntimeManager>) -> Self {
        self.manager = manager;
        self
    }
}

impl PrepareChannel for LocalPrepareChannel {
    fn prepare(&self, request: PrepareRequest, budget: Duration) -> io::Result<PrepareOutcome> {
        // Read the session now, not at construction: a sidecar restart between two
        // preparations must be visible here, and the old session's pending table is
        // already closed by then.
        let Some(session) = self.sessions.current() else {
            return Ok(PrepareOutcome::Unknown {
                reason: "this shell has no local management session".to_string(),
            });
        };
        let transport = self.sessions.transport();
        let reply = session
            .prepare(transport.as_ref(), &request, budget)
            .map_err(io::Error::other)?;
        let frame = match reply {
            crate::runtime_management::ManagementReply::Answered(frame) => frame,
            // No answer is `Unknown`, not `Blocked`: the project may well be fine,
            // and calling it blocked would invite a retry that starts it twice.
            crate::runtime_management::ManagementReply::Abandoned => {
                return Ok(PrepareOutcome::Unknown {
                    reason: "the local management channel did not answer the preparation"
                        .to_string(),
                });
            }
        };
        if let Some(error) = frame.get("error").and_then(|error| error.as_str()) {
            return Ok(PrepareOutcome::Blocked {
                reason: error.to_string(),
            });
        }
        // The epoch the session fixed is the only one this shell may trust: a frame
        // reporting a different one describes a generation we are not talking to, so
        // its result is not ours to apply.
        if let Some(epoch) = frame.get("sidecar_epoch").and_then(|epoch| epoch.as_str()) {
            if epoch != session.epoch() {
                return Ok(PrepareOutcome::Unknown {
                    reason: "the preparation answer came from a different sidecar generation"
                        .to_string(),
                });
            }
            if let Some(manager) = self.manager.upgrade() {
                manager.note_sidecar_epoch(epoch);
            }
        }
        let Some(result) = frame.get("result") else {
            return Ok(PrepareOutcome::Unknown {
                reason: "the preparation answer carried neither a result nor an error".to_string(),
            });
        };
        serde_json::from_value(result.clone()).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("the preparation outcome was not understood: {error}"),
            )
        })
    }
}

/// Sends one `_shell/prepare` to this shell's own sidecar and waits for its answer.
///
/// A trait so the state machine can be tested without a sidecar: the transitions
/// (register only after Connected, hold before heartbeat, release on an abandoned
/// attempt) are what must be pinned, and none of them needs a process.
pub trait PrepareChannel: Send + Sync + 'static {
    fn prepare(&self, request: PrepareRequest, budget: Duration) -> io::Result<PrepareOutcome>;
}

/// Sends one legacy-migration request to this shell's own sidecar.
///
/// Deliberately *not* a `ControlBackend` method: migration talks to the sidecar's
/// private management face, not to the guardian, because it is the sidecar that
/// owns the journal and the platform probe. Keeping it a separate trait also means
/// a test can prove the "one confirmation, then poll the same id" rule without a
/// process, which is the rule that costs a user their running work when it breaks.
pub trait MigrationChannel: Send + Sync + 'static {
    /// One request. `Ok(None)` means the session gave no answer (closed, replaced,
    /// or out of budget); the caller must not retry a confirmation in that case.
    fn call(
        &self,
        method: &str,
        params: serde_json::Value,
        budget: Duration,
    ) -> io::Result<Option<serde_json::Value>>;
}

/// The packaged shell's migration channel: the private stdio session of its own
/// local sidecar.
pub struct LocalMigrationChannel {
    sessions: Arc<dyn ManagementSessions>,
    manager: std::sync::Weak<DesktopRuntimeManager>,
}

impl LocalMigrationChannel {
    pub fn new(sessions: Arc<dyn ManagementSessions>) -> Self {
        Self {
            sessions,
            manager: std::sync::Weak::new(),
        }
    }

    pub fn with_manager(mut self, manager: std::sync::Weak<DesktopRuntimeManager>) -> Self {
        self.manager = manager;
        self
    }
}

impl MigrationChannel for LocalMigrationChannel {
    fn call(
        &self,
        method: &str,
        params: serde_json::Value,
        budget: Duration,
    ) -> io::Result<Option<serde_json::Value>> {
        // Looked up per call, never cached: a sidecar restart between a preview and
        // its confirmation must be visible here, and the old pending table is closed
        // by then.
        let Some(session) = self.sessions.current() else {
            return Err(io::Error::other(
                "this shell has no local management session to migrate with",
            ));
        };
        let transport = self.sessions.transport();
        let reply = session
            .call(transport.as_ref(), "migration", method, params, budget)
            .map_err(io::Error::other)?;
        let frame = match reply {
            crate::runtime_management::ManagementReply::Answered(frame) => frame,
            crate::runtime_management::ManagementReply::Abandoned => return Ok(None),
        };
        if let Some(error) = frame.get("error").and_then(|error| error.as_str()) {
            return Err(io::Error::other(error.to_string()));
        }
        // An answer from another generation is not ours to apply: it was computed
        // by a process this shell is no longer talking to.
        if let Some(epoch) = frame.get("sidecar_epoch").and_then(|epoch| epoch.as_str()) {
            if epoch != session.epoch() {
                return Ok(None);
            }
            if let Some(manager) = self.manager.upgrade() {
                manager.note_sidecar_epoch(epoch);
            }
        }
        match frame.get("result") {
            Some(result) => Ok(Some(result.clone())),
            None => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the migration answer carried neither a result nor an error",
            )),
        }
    }
}

/// Owns this App run's identity and its per-project registrations.
pub struct DesktopRuntimeManager {
    client_run_id: String,
    projects: Mutex<BTreeMap<String, RuntimeScope>>,
    backend: Arc<dyn ControlBackend>,
    /// Once quitting has begun no new project may be registered: it would
    /// outlive the quit that is already asking every runtime to let go.
    quitting: AtomicBool,
    /// The artifact this shell shipped with, verified once at startup.
    ///
    /// Resolved once rather than per request: verifying means hashing the sidecar
    /// binary, and doing that on every status poll would cost more than the poll.
    artifact: Option<ArtifactIdentity>,
    /// Every scope this shell has resolved for an open project, lease or not.
    ///
    /// Kept separately from `projects` because "the user has this project open" and
    /// "a guardian gave us a lease" are different facts. An embedded or legacy
    /// runtime answers no registration, yet its status must still be readable --
    /// those are exactly the runtimes whose version banner says "unknown" or "this
    /// old runtime has no exit guarantees". Keying the read on `projects` would make
    /// the shell unable to describe the cases it most needs to report.
    known: Mutex<BTreeMap<String, RuntimeScope>>,
    /// Operations this shell started, keyed by scope.
    ///
    /// Kept here, not in the WebView, because a WebView reload must not lose the
    /// answer to "did my restart happen?". The id is chosen before the click, so a
    /// lost response is recovered by querying this id -- never by clicking again.
    operations: Mutex<BTreeMap<String, OperationId>>,
    /// Per-scope acquisition result: what was attempted, what is held, what is
    /// ready.
    ///
    /// Kept apart from `projects` (which answers "does the quit have something to
    /// release") because the two must be able to disagree. A compatible build
    /// mismatch holds a lease while remaining blocked; a lease that was just
    /// registered is held but not yet ready.
    states: Mutex<BTreeMap<String, ProjectState>>,
    /// Serialises attempts per scope so two concurrent ensure calls share one
    /// preparation instead of racing to start the runtime twice.
    attempts: Mutex<BTreeMap<String, Arc<AttemptSlot>>>,
    /// The local management channel, when this shell has one.
    ///
    /// `None` is a real state: a build with no sidecar (or a test that only
    /// exercises the lease half) cannot prepare anything, and `ensure_project`
    /// reports that as blocked rather than inventing a runtime.
    ///
    /// Behind a lock rather than a plain field because the channel needs a
    /// `Weak` handle on this very manager (to record the sidecar epoch it sees),
    /// and the manager must therefore exist before the channel does.
    prepare: Mutex<Option<Arc<dyn PrepareChannel>>>,
    /// The legacy-migration channel, when this shell has one.
    ///
    /// `None` is a real state and must stay distinguishable from "there is nothing
    /// to migrate": the latter is a preview that lists nothing, the former is a
    /// refusal to answer at all.
    migration: Mutex<Option<Arc<dyn MigrationChannel>>>,
    /// Legacy migrations this shell started, keyed by scope.
    ///
    /// Kept apart from `operations` on purpose: that map answers "is a *version
    /// switch* in flight here?", and a migration id in it would make the switch
    /// dialog poll an id the switch machinery never issued.
    migrations: Mutex<BTreeMap<String, String>>,
    /// The authenticated epoch of the sidecar this shell bootstrapped.
    ///
    /// Set from the bootstrap answer, never from an incoming request: a caller
    /// that could name its own epoch could make a stale sidecar look current.
    sidecar_epoch: Mutex<Option<String>>,
    /// Which consumers currently want a plugin service, per `(scope, plugin)`.
    ///
    /// The native side is where demand is aggregated because the guardian's demand
    /// table stops a service when its *last* holder releases. A WebView that sent a
    /// release per view would stop a service another view still uses, so the page
    /// names consumers and the shell decides when "the last one" has left.
    ///
    /// Keeping it here also means a WebView reload does not drop demand it never
    /// released: the consumers die with the page, but the shell still knows which
    /// ones it was told about.
    plugin_demand: Mutex<BTreeMap<String, BTreeMap<String, std::collections::BTreeSet<String>>>>,
}

impl DesktopRuntimeManager {
    pub fn new(client_run_id: impl Into<String>) -> Self {
        Self::with_backend(client_run_id, Arc::new(SocketBackend::new()))
    }

    /// Test constructor: the same manager, a different control channel.
    pub fn with_backend(
        client_run_id: impl Into<String>,
        backend: Arc<dyn ControlBackend>,
    ) -> Self {
        Self {
            client_run_id: client_run_id.into(),
            projects: Mutex::new(BTreeMap::new()),
            backend,
            quitting: AtomicBool::new(false),
            artifact: None,
            known: Mutex::new(BTreeMap::new()),
            operations: Mutex::new(BTreeMap::new()),
            states: Mutex::new(BTreeMap::new()),
            attempts: Mutex::new(BTreeMap::new()),
            prepare: Mutex::new(None),
            migration: Mutex::new(None),
            migrations: Mutex::new(BTreeMap::new()),
            sidecar_epoch: Mutex::new(None),
            plugin_demand: Mutex::new(BTreeMap::new()),
        }
    }

    /// The same manager, declaring the artifact this shell shipped with.
    ///
    /// `None` is a real answer: a `tauri dev` build has no manifest, and the shell
    /// must not then invent an expectation out of a version number.
    pub fn with_artifact(mut self, artifact: Option<ArtifactIdentity>) -> Self {
        self.artifact = artifact;
        self
    }

    /// The same manager, preparing projects over a local management channel.
    pub fn with_prepare(self, prepare: Arc<dyn PrepareChannel>) -> Self {
        self.attach_prepare(prepare);
        self
    }

    /// Installs the preparation channel after the manager is already shared.
    ///
    /// Needed because the real channel holds a `Weak` handle on this manager: it
    /// records the sidecar epoch, and the epoch belongs to the manager, so the
    /// manager has to exist first.
    pub fn attach_prepare(&self, prepare: Arc<dyn PrepareChannel>) {
        if let Ok(mut guard) = self.prepare.lock() {
            *guard = Some(prepare);
        }
    }

    /// Installs the migration channel. Same reason as `attach_prepare`: it needs a
    /// `Weak` handle on this manager to record the sidecar epoch.
    pub fn attach_migration(&self, migration: Arc<dyn MigrationChannel>) {
        if let Ok(mut guard) = self.migration.lock() {
            *guard = Some(migration);
        }
    }

    /// Replaces the migration channel. Only tests need this.
    pub fn with_migration(self, migration: Arc<dyn MigrationChannel>) -> Self {
        self.attach_migration(migration);
        self
    }

    /// Records the migration operation this shell just started for `scope`.
    pub fn note_migration_operation(&self, scope_key: &str, operation_id: &str) {
        if let Ok(mut guard) = self.migrations.lock() {
            guard.insert(scope_key.to_string(), operation_id.to_string());
        }
    }

    /// The migration this shell last started for `scope`, if any.
    ///
    /// Exists so a WebView reload resumes polling instead of forgetting that a
    /// migration is in flight -- and, more importantly, so it does not offer a
    /// second confirmation for one already sent.
    pub fn migration_operation_for(&self, scope_key: &str) -> Option<String> {
        self.migrations
            .lock()
            .ok()
            .and_then(|guard| guard.get(scope_key).cloned())
    }

    /// Forgets a finished migration. A terminal phase is the only thing that clears
    /// it: an in-flight one must stay visible so it is not confirmed twice.
    pub fn forget_migration(&self, scope_key: &str) {
        if let Ok(mut guard) = self.migrations.lock() {
            guard.remove(scope_key);
        }
    }

    /// The migration channel, when this build has one.
    pub fn migration_channel(&self) -> Option<Arc<dyn MigrationChannel>> {
        self.migration.lock().ok().and_then(|guard| guard.clone())
    }

    /// Records that one consumer wants `plugin` for `scope`.
    ///
    /// Returns whether the plugin became wanted *at all*, which is the only moment
    /// the guardian has to be told to acquire: a second consumer is a local fact
    /// and does not change the service.
    pub fn note_plugin_demand(&self, scope_key: &str, plugin: &str, consumer: &str) -> bool {
        let Ok(mut demand) = self.plugin_demand.lock() else {
            return false;
        };
        let consumers = demand
            .entry(scope_key.to_string())
            .or_default()
            .entry(plugin.to_string())
            .or_default();
        let was_wanted = !consumers.is_empty();
        consumers.insert(consumer.to_string());
        !was_wanted
    }

    /// Removes one consumer's demand.
    ///
    /// Returns whether that was the last consumer of that plugin, i.e. whether the
    /// guardian must now be told to release. A consumer that never registered is
    /// "not the last one", so an unbalanced release cannot stop a shared service.
    pub fn drop_plugin_demand(&self, scope_key: &str, plugin: &str, consumer: &str) -> bool {
        let Ok(mut demand) = self.plugin_demand.lock() else {
            return false;
        };
        let Some(plugins) = demand.get_mut(scope_key) else {
            return false;
        };
        let Some(consumers) = plugins.get_mut(plugin) else {
            return false;
        };
        if !consumers.remove(consumer) {
            return false;
        }
        if consumers.is_empty() {
            plugins.remove(plugin);
            if plugins.is_empty() {
                demand.remove(scope_key);
            }
            return true;
        }
        false
    }

    /// Every plugin this shell believes is wanted for `scope`, for reconciliation.
    pub fn wanted_plugins(&self, scope_key: &str) -> Vec<String> {
        self.plugin_demand
            .lock()
            .map(|demand| {
                demand
                    .get(scope_key)
                    .map(|plugins| plugins.keys().cloned().collect())
                    .unwrap_or_default()
            })
            .unwrap_or_default()
    }

    /// Forgets a whole scope's demand, without telling anyone.
    ///
    /// Used by the quit path: the releases it sends already stop the services, and
    /// re-sending a demand release after the lease is gone would only fail.
    pub fn forget_plugin_demand(&self, scope_key: &str) {
        if let Ok(mut demand) = self.plugin_demand.lock() {
            demand.remove(scope_key);
        }
    }

    /// The artifact this shell expects, when it could verify one.
    pub fn artifact(&self) -> Option<&ArtifactIdentity> {
        self.artifact.as_ref()
    }

    /// The operation this shell started for `scope`, if any.
    pub fn operation_for(&self, scope_key: &str) -> Option<OperationId> {
        self.operations
            .lock()
            .ok()
            .and_then(|guard| guard.get(scope_key).cloned())
    }

    /// One status read. Read-only: it neither registers nor renews anything.
    pub async fn status(
        &self,
        scope: &RuntimeScope,
        budget: Duration,
    ) -> io::Result<Option<RuntimeStatusSnapshot>> {
        let backend = Arc::clone(&self.backend);
        let scope = scope.clone();
        run_bounded(budget, move || backend.status(&scope, budget)).await
    }

    /// A read-only preview of replacing `scope`'s runtime with this shell's
    /// artifact.
    ///
    /// The target is always `self.artifact`: a WebView must not be able to name an
    /// executable, because "restart into this binary you chose" is code execution
    /// with extra steps. An unverified shell has no target, and says so.
    /// The live lease this App holds for one project, for a surface that must speak
    /// to the runtime as itself.
    ///
    /// Deliberately not a general accessor: recovery is owner-bound at the runtime,
    /// so the caller presents the App's own grant rather than asking the sidecar to
    /// obtain one. A project with no live lease gets "open the project first", which
    /// is the only fix the caller can apply.
    pub fn lease_for_recovery(
        &self,
        scope: &RuntimeScope,
    ) -> io::Result<Arc<Mutex<yi_agent_lifecycle::client::FrontendLease>>> {
        self.backend.lease_for_scope(scope)
    }

    pub async fn preview(
        &self,
        scope: &RuntimeScope,
        budget: Duration,
    ) -> io::Result<RestartPreview> {
        let target = self.artifact.clone().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "this build has no verified runtime artifact, so it cannot be a restart target",
            )
        })?;
        let backend = Arc::clone(&self.backend);
        let id = self.client_run_id.clone();
        let scope = scope.clone();
        run_bounded(budget, move || backend.preview(&id, &scope, target, budget)).await
    }

    /// Confirms a preview, binding it to an operation id chosen by the caller.
    ///
    /// The id is recorded **before** the request goes out. If the answer is lost,
    /// the same id is queried instead of the restart being requested twice, and a
    /// reload still finds it.
    pub async fn confirm(
        &self,
        scope: &RuntimeScope,
        token: &str,
        operation_id: &str,
        budget: Duration,
    ) -> io::Result<OperationId> {
        self.operations
            .lock()
            .map_err(|_| poisoned())?
            .insert(scope.key.clone(), operation_id.to_string());
        let backend = Arc::clone(&self.backend);
        let id = self.client_run_id.clone();
        let scope_for_io = scope.clone();
        let key = scope.key.clone();
        let token = token.to_string();
        let operation = operation_id.to_string();
        let result = run_bounded(budget, move || {
            backend.confirm(&id, &scope_for_io, &token, &operation, budget)
        })
        .await;
        if result.is_err() {
            // A failed request is not an operation: keeping it would make the UI
            // poll an id the runtime never accepted.
            if let Ok(mut guard) = self.operations.lock() {
                if guard.get(&key).map(String::as_str) == Some(operation_id) {
                    guard.remove(&key);
                }
            }
        }
        result
    }

    /// One read of an operation's progress.
    pub async fn operation(
        &self,
        scope: &RuntimeScope,
        operation_id: &str,
        budget: Duration,
    ) -> io::Result<Option<OperationStatus>> {
        let backend = Arc::clone(&self.backend);
        let id = self.client_run_id.clone();
        let scope = scope.clone();
        let operation = operation_id.to_string();
        run_bounded(budget, move || {
            backend.operation(&id, &scope, &operation, budget)
        })
        .await
    }

    /// The App's identity for this run. Stable across sidecar restarts by
    /// construction: it is generated once, when the shell starts.
    pub fn client_run_id(&self) -> &str {
        &self.client_run_id
    }

    /// The projects this run currently holds.
    pub fn registered_projects(&self) -> Vec<RuntimeScope> {
        self.projects
            .lock()
            .map(|guard| guard.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Records that `scope` belongs to a project the user has open.
    ///
    /// This is the readable half of opening a project, and it is recorded before the
    /// lease is requested: whether a lease exists is the runtime's answer, not a
    /// precondition for describing it.
    pub fn note_open(&self, scope: &RuntimeScope) {
        if let Ok(mut guard) = self.known.lock() {
            guard.insert(scope.key.clone(), scope.clone());
        }
    }

    /// Forgets a scope the user has closed.
    pub fn forget_open(&self, scope_key: &str) {
        if let Ok(mut guard) = self.known.lock() {
            guard.remove(scope_key);
        }
    }

    /// Every scope this shell can describe: open projects, plus any still leased.
    pub fn known_scopes(&self) -> Vec<RuntimeScope> {
        let mut all: BTreeMap<String, RuntimeScope> = self
            .known
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        for scope in self.registered_projects() {
            all.entry(scope.key.clone()).or_insert(scope);
        }
        all.into_values().collect()
    }

    /// Registers the App's lease for one project.
    ///
    /// Idempotent: switching between two windows of the same project must not
    /// create a second client. A project the user never actually uses is never
    /// registered — a historical entry in an index must not keep a runtime alive.
    pub async fn acquire_project(&self, scope: RuntimeScope, mode: LeaseMode) -> io::Result<()> {
        if self.quitting.load(Ordering::SeqCst) {
            return Err(io::Error::other(
                "the app is quitting; no new project lease may be registered",
            ));
        }
        let key = scope.key.clone();
        {
            let guard = self.projects.lock().map_err(|_| poisoned())?;
            if guard.contains_key(&key) {
                return Ok(());
            }
        }

        let backend = Arc::clone(&self.backend);
        let id = self.client_run_id.clone();
        let scope_for_io = scope.clone();
        let held = run_bounded(ACQUIRE_BUDGET, move || {
            backend.register(&id, &scope_for_io, mode, ACQUIRE_BUDGET)
        })
        .await
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("could not register project {}: {error}", scope.key),
            )
        })?;

        // Re-check the quit flag: a quit that began while the registration was in
        // flight would otherwise be followed by a lease nobody releases.
        if self.quitting.load(Ordering::SeqCst) {
            if held {
                self.release_quietly(&scope).await;
            }
            return Err(io::Error::other(
                "the app began quitting while this project was being registered",
            ));
        }

        // No guardian served this scope, so there is no lease: do not record it as
        // something the quit must release. `registered_projects` then reports only
        // work that really has an owner to let go of.
        if !held {
            return Ok(());
        }

        // Recorded *before* the heartbeat: from this moment the quit has something
        // to release, and a heartbeat failure must not make the lease disappear
        // from the drain's point of view.
        self.projects
            .lock()
            .map_err(|_| poisoned())?
            .insert(key.clone(), scope.clone());

        let backend = Arc::clone(&self.backend);
        let scope_for_io = scope.clone();
        if let Err(error) = run_bounded(HEARTBEAT_START_BUDGET, move || {
            backend.start_heartbeat(&scope_for_io)
        })
        .await
        {
            // A lease that nobody renews will be expired by the guardian, but until
            // then the shell must not pretend it is usable. Dropping the record
            // makes the state honest and leaves the reclaim to the guardian's own
            // expiry, which is already the designed outcome for an abandoned client.
            self.projects.lock().map_err(|_| poisoned())?.remove(&key);
            self.release_quietly(&scope).await;
            return Err(io::Error::other(format!(
                "project {} was registered but its lease cannot be renewed: {error}",
                scope.key
            )));
        }
        Ok(())
    }

    /// Releases one scope, ignoring every error.
    ///
    /// Used on paths where the caller is already reporting a failure: the release
    /// is best-effort cleanup, and its error must not replace the reason the
    /// attempt was abandoned.
    async fn release_quietly(&self, scope: &RuntimeScope) {
        let backend = Arc::clone(&self.backend);
        let id = self.client_run_id.clone();
        let scope_for_io = scope.clone();
        let _ = run_bounded(ACQUIRE_BUDGET, move || {
            backend.release(&id, &scope_for_io, ACQUIRE_BUDGET)
        })
        .await;
    }

    /// Prepares one project and, only if that succeeds, takes a real lease.
    ///
    /// The three steps are ordered because each depends on the previous one being
    /// true, not merely having been requested:
    ///
    /// 1. `prepare` — is there a managed runtime for this scope, and is it ours?
    /// 2. `register` — the guardian grants a lease; the shell records it as held.
    /// 3. `start_heartbeat` — the lease is being renewed; only then is it ready.
    ///
    /// `managedReady` is reached only when all three hold *and* the identity is
    /// still the one preparation observed. Any failure leaves `ready == false`;
    /// a held lease stays held (and stays in the quit barrier) unless the attempt
    /// itself is abandoned.
    pub async fn ensure_project(
        &self,
        scope: &RuntimeScope,
        budget: Duration,
    ) -> Result<ProjectAcquireView, String> {
        if self.quitting.load(Ordering::SeqCst) {
            return Ok(blocked_view(
                &scope.key,
                "",
                "the app is quitting; no project may be prepared",
                RetryClass::DoNotRetry,
            ));
        }

        // One attempt per scope at a time. Two windows opening the same project
        // must share one preparation, not start the runtime twice.
        let slot = {
            let mut attempts = self
                .attempts
                .lock()
                .map_err(|_| "the project attempt table is poisoned".to_string())?;
            Arc::clone(
                attempts
                    .entry(scope.key.clone())
                    .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
            )
        };
        let _serialised = slot.lock().await;

        // Already there: a repeat call must not prepare or register again.
        if let Some(state) = self.state_for(&scope.key) {
            if state.ready && self.backend.lease_is_live(scope) {
                return Ok(state.view);
            }
        }

        let attempt_id = uuid::Uuid::new_v4().to_string();
        // Admission is published before any work starts, so a caller arriving now
        // sees `starting` rather than a stale `blocked` from the previous attempt.
        self.record_view(
            scope,
            &attempt_id,
            false,
            false,
            starting_view(&scope.key, &attempt_id),
        );

        let channel = self.prepare.lock().ok().and_then(|guard| guard.clone());
        let Some(channel) = channel else {
            return Ok(self.settle(
                scope,
                &attempt_id,
                blocked_view(
                    &scope.key,
                    &attempt_id,
                    "this shell has no local management channel to prepare a project",
                    RetryClass::DoNotRetry,
                ),
            ));
        };
        let Some(expected) = self.artifact.clone() else {
            // No verified artifact means the build identity cannot be compared, and
            // a runtime whose build nobody can name must not be declared ready.
            return Ok(self.settle(
                scope,
                &attempt_id,
                blocked_view(
                    &scope.key,
                    &attempt_id,
                    "this build has no verified runtime artifact, so its runtime cannot be identified",
                    RetryClass::DoNotRetry,
                ),
            ));
        };

        let request = PrepareRequest {
            scope: scope.clone(),
            expected: expected.clone(),
            attempt_id: attempt_id.clone(),
        };
        let prepared = {
            let channel = Arc::clone(&channel);
            let request = request.clone();
            tokio::task::spawn_blocking(move || channel.prepare(request, budget)).await
        };
        let outcome = match prepared {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(error)) => {
                return Ok(self.settle(
                    scope,
                    &attempt_id,
                    unknown_view(
                        &scope.key,
                        &attempt_id,
                        format!("the preparation channel did not answer: {error}"),
                    ),
                ));
            }
            Err(join) => {
                return Ok(self.settle(
                    scope,
                    &attempt_id,
                    unknown_view(
                        &scope.key,
                        &attempt_id,
                        format!("the preparation task failed: {join}"),
                    ),
                ));
            }
        };

        // Whether this outcome permits taking a lease at all. A version mismatch
        // that is protocol-compatible may hold one for the existing preview and
        // confirm flow; it still may not be ready.
        let lease_permitted = match &outcome {
            PrepareOutcome::Connected(identity) => {
                if identity.scope_key != scope.key {
                    return Ok(self.settle(
                        scope,
                        &attempt_id,
                        blocked_view(
                            &scope.key,
                            &attempt_id,
                            format!(
                                "the runtime answered for a different scope ({})",
                                identity.scope_key
                            ),
                            RetryClass::DoNotRetry,
                        ),
                    ));
                }
                true
            }
            PrepareOutcome::VersionChoiceRequired(identity) => {
                if identity.scope_key != scope.key {
                    return Ok(self.settle(
                        scope,
                        &attempt_id,
                        blocked_view(
                            &scope.key,
                            &attempt_id,
                            format!(
                                "the runtime answered for a different scope ({})",
                                identity.scope_key
                            ),
                            RetryClass::DoNotRetry,
                        ),
                    ));
                }
                // Only a verified-compatible control protocol earns a held lease:
                // preview/confirm speak it, and nothing else may start.
                protocols_overlap(&expected, identity)
            }
            _ => false,
        };

        let mut held = false;
        if lease_permitted {
            let backend = Arc::clone(&self.backend);
            let id = self.client_run_id.clone();
            let scope_for_io = scope.clone();
            let registered = run_bounded(budget, move || {
                backend.register(&id, &scope_for_io, LeaseMode::Business, budget)
            })
            .await;
            match registered {
                Ok(true) => {
                    // Held is recorded immediately: from here on a quit must release
                    // this lease even if everything after it fails.
                    held = true;
                    if let Ok(mut guard) = self.projects.lock() {
                        guard.insert(scope.key.clone(), scope.clone());
                    }
                }
                Ok(false) => {
                    return Ok(self.settle(
                        scope,
                        &attempt_id,
                        unknown_view(
                            &scope.key,
                            &attempt_id,
                            "the runtime answered a preparation but no guardian accepted a lease",
                        ),
                    ));
                }
                Err(error) => {
                    return Ok(self.settle(
                        scope,
                        &attempt_id,
                        blocked_view(
                            &scope.key,
                            &attempt_id,
                            format!("the runtime refused a lease: {error}"),
                            RetryClass::Backoff,
                        ),
                    ));
                }
            }
        }

        // A quit may have started while registration was in flight. The lease is
        // released at once and the result is refused, so a late attempt cannot
        // hand the user a runtime the quit is already reclaiming.
        if held && self.quitting.load(Ordering::SeqCst) {
            self.release_quietly(scope).await;
            self.forget_held(&scope.key);
            return Ok(self.settle(
                scope,
                &attempt_id,
                blocked_view(
                    &scope.key,
                    &attempt_id,
                    "the app began quitting while this project was being registered",
                    RetryClass::DoNotRetry,
                ),
            ));
        }

        // The view for a non-ready outcome is decided before the heartbeat, so a
        // blocked project never touches a renewal it does not need.
        let not_ready = match &outcome {
            PrepareOutcome::Connected(identity) => {
                let _ = identity;
                None
            }
            PrepareOutcome::VersionChoiceRequired(identity) => Some(blocked_view(
                &scope.key,
                &attempt_id,
                format!(
                    "a runtime of version {} is serving this project; choosing to switch updates it",
                    identity
                        .daemon_display_version
                        .clone()
                        .unwrap_or_else(|| "an unknown build".to_string())
                ),
                RetryClass::UserAction,
            )),
            PrepareOutcome::LegacyChoiceRequired { reason } => Some(ProjectAcquireView {
                scope_key: scope.key.clone(),
                attempt_id: attempt_id.clone(),
                state: AcquireState::LegacyChoiceRequired,
                reason: Some(reason.clone()),
                retry: RetryClass::UserAction,
            }),
            PrepareOutcome::Blocked { reason } => Some(blocked_view(
                &scope.key,
                &attempt_id,
                reason.clone(),
                RetryClass::Backoff,
            )),
            PrepareOutcome::Unknown { reason } => Some(unknown_view(
                &scope.key,
                &attempt_id,
                reason.clone(),
            )),
        };

        // Renewal starts the moment a lease is held, not when readiness is decided.
        // A compatible version mismatch keeps its lease while blocked so that
        // preview and confirm can speak to the runtime the user is deciding about --
        // but a person may take minutes to read that, and `LEASE_EXPIRY` is seconds.
        // Renewing only on the ready path would hand the switch flow a lease that
        // had already expired.
        if held {
            let backend = Arc::clone(&self.backend);
            let scope_for_io = scope.clone();
            if let Err(error) = run_bounded(HEARTBEAT_START_BUDGET, move || {
                backend.start_heartbeat(&scope_for_io)
            })
            .await
            {
                self.release_quietly(scope).await;
                self.forget_held(&scope.key);
                return Ok(self.settle(
                    scope,
                    &attempt_id,
                    blocked_view(
                        &scope.key,
                        &attempt_id,
                        format!("the lease was granted but could not be renewed: {error}"),
                        RetryClass::Backoff,
                    ),
                ));
            }
        }

        if let Some(view) = not_ready {
            return Ok(self.settle_with(scope, &attempt_id, held, view));
        }

        // A queued grant is not a business lease. The guardian hands one out while a
        // switch is in progress, and treating it as ready would let work run on a
        // runtime that is actively being replaced. The lease is still the quit's to
        // release, so it stays held.
        if self.backend.lease_mode(scope) != Some(LeaseMode::Business) {
            self.forget_ready(&scope.key);
            return Ok(self.settle_with(
                scope,
                &attempt_id,
                held,
                blocked_view(
                    &scope.key,
                    &attempt_id,
                    "the runtime queued this client behind a switch instead of granting a business lease",
                    RetryClass::Backoff,
                ),
            ));
        }

        // Last gate: the identity must still be the one preparation saw. A daemon
        // that was replaced between the answer and this point is a different
        // runtime, and ready must not describe it.
        if !self.identity_is_current(scope, &outcome, budget).await {
            self.forget_ready(&scope.key);
            return Ok(self.settle_with(
                scope,
                &attempt_id,
                held,
                unknown_view(
                    &scope.key,
                    &attempt_id,
                    "the runtime identity changed after the lease was taken",
                ),
            ));
        }
        if !self.backend.lease_is_live(scope) {
            self.forget_ready(&scope.key);
            return Ok(self.settle_with(
                scope,
                &attempt_id,
                held,
                blocked_view(
                    &scope.key,
                    &attempt_id,
                    "the granted lease is not being renewed, so this project is not ready",
                    RetryClass::Backoff,
                ),
            ));
        }

        Ok(self.settle_with(
            scope,
            &attempt_id,
            held,
            ProjectAcquireView {
                scope_key: scope.key.clone(),
                attempt_id: attempt_id.clone(),
                state: AcquireState::ManagedReady,
                reason: None,
                retry: RetryClass::DoNotRetry,
            },
        ))
    }

    /// Whether the runtime serving `scope` is still the generation preparation saw.
    ///
    /// Only a positive answer counts. A status read that fails or times out leaves
    /// the caller unable to prove sameness, and "cannot prove" is not "current".
    async fn identity_is_current(
        &self,
        scope: &RuntimeScope,
        outcome: &PrepareOutcome,
        budget: Duration,
    ) -> bool {
        let PrepareOutcome::Connected(prepared) = outcome else {
            return false;
        };
        let backend = Arc::clone(&self.backend);
        let scope_for_io = scope.clone();
        let read = run_bounded(budget, move || backend.status(&scope_for_io, budget)).await;
        match read {
            Ok(Some(snapshot)) => {
                snapshot.identity.daemon_instance_id == prepared.daemon_instance_id
                    && snapshot.identity.scope_key == prepared.scope_key
            }
            _ => false,
        }
    }

    /// Records a view, keeping the previous held/ready flags unless told otherwise.
    fn record_view(
        &self,
        scope: &RuntimeScope,
        attempt_id: &str,
        held: bool,
        ready: bool,
        view: ProjectAcquireView,
    ) {
        if let Ok(mut guard) = self.states.lock() {
            guard.insert(
                scope.key.clone(),
                ProjectState {
                    scope: scope.clone(),
                    attempt_id: attempt_id.to_string(),
                    view,
                    held,
                    ready,
                },
            );
        }
    }

    /// Settles an attempt that holds nothing.
    fn settle(
        &self,
        scope: &RuntimeScope,
        attempt_id: &str,
        view: ProjectAcquireView,
    ) -> ProjectAcquireView {
        // A held lease from an earlier attempt is *not* cleared here: this
        // attempt's failure says nothing about a lease another attempt took.
        let (held, ready) = self
            .state_for(&scope.key)
            .map(|state| (state.held, state.ready))
            .unwrap_or((false, false));
        self.record_view(scope, attempt_id, held, ready, view.clone());
        view
    }

    /// Settles an attempt whose own outcome decides what is held.
    fn settle_with(
        &self,
        scope: &RuntimeScope,
        attempt_id: &str,
        held: bool,
        view: ProjectAcquireView,
    ) -> ProjectAcquireView {
        let ready = view.state == AcquireState::ManagedReady;
        self.record_view(scope, attempt_id, held, ready, view.clone());
        view
    }

    /// Records that this scope no longer holds a lease.
    ///
    /// Both halves must go: `states` is what readiness reads, and `projects` is
    /// what the quit releases. Clearing only the first would leave the drain
    /// asking a runtime to release a lease this shell already released.
    fn forget_held(&self, scope_key: &str) {
        if let Ok(mut guard) = self.states.lock() {
            if let Some(state) = guard.get_mut(scope_key) {
                state.held = false;
                state.ready = false;
            }
        }
        if let Ok(mut guard) = self.projects.lock() {
            guard.remove(scope_key);
        }
    }

    fn forget_ready(&self, scope_key: &str) {
        if let Ok(mut guard) = self.states.lock() {
            if let Some(state) = guard.get_mut(scope_key) {
                state.ready = false;
            }
        }
    }

    /// One scope's acquisition state, if it has been attempted.
    fn state_for(&self, scope_key: &str) -> Option<ProjectState> {
        self.project_state(scope_key)
    }

    /// One scope's acquisition state, if it has been attempted.
    pub fn project_state(&self, scope_key: &str) -> Option<ProjectState> {
        self.states
            .lock()
            .ok()
            .and_then(|guard| guard.get(scope_key).cloned())
    }

    /// Whether `scope` reached `lifecycleReady` for this attempt.
    ///
    /// Re-checked against the live lease: a heartbeat that died after ready was
    /// recorded must not keep authorizing work, because the guardian has by then
    /// withdrawn the client.
    pub fn is_lifecycle_ready(&self, scope: &RuntimeScope) -> bool {
        let ready = self
            .project_state(&scope.key)
            .map(|state| state.ready)
            .unwrap_or(false);
        if !ready {
            return false;
        }
        if !self.backend.lease_is_live(scope) {
            self.forget_ready(&scope.key);
            return false;
        }
        true
    }

    /// Notes the epoch of the sidecar this shell is currently talking to.
    ///
    /// The epoch is what makes a root intent attributable to one app-server
    /// generation: a request forwarded by a previous sidecar must not be honoured
    /// by the next one, even though both speak for the same App run.
    pub fn note_sidecar_epoch(&self, epoch: &str) {
        if let Ok(mut guard) = self.sidecar_epoch.lock() {
            *guard = Some(epoch.to_string());
        }
    }

    /// Turns one session's intent into a root authorization, or refuses.
    ///
    /// Every condition here is a precondition for the *capability*, not a
    /// convenience: a live lease (the runtime still counts this shell), lifecycle
    /// readiness (the identity is confirmed), the authenticated sidecar epoch (the
    /// request comes from the session we bootstrapped), and the App's own client
    /// run id (nobody may act for another client). Deliberately not a Tauri
    /// command: the page must never be able to ask for a root capability.
    /// Authorizes one conversation's root, returning the entry the sidecar installs.
    ///
    /// The entry, not the bare `RootAuthorization`: the sidecar's store is keyed by
    /// scope, generation, client, conversation and root, and the authorization the
    /// guardian hands back does not carry the conversation. Filling it in here is
    /// honest -- this side is the one that received the intent and validated it -- and
    /// keeps the secret in one place instead of asking the caller to re-derive a key.
    pub async fn authorize_intent(
        &self,
        request: RootIntentRequest,
        deadline: Deadline,
    ) -> Result<yi_agent_lifecycle::production::RootAuthorityEntry, String> {
        if self.quitting.load(Ordering::SeqCst) {
            return Err("the app is quitting; no root may be authorized".to_string());
        }
        if request.client_run_id != self.client_run_id {
            return Err("this intent names a different client run".to_string());
        }
        let expected_epoch = self
            .sidecar_epoch
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
            .ok_or_else(|| "this shell has no authenticated sidecar epoch".to_string())?;
        if request.sidecar_epoch != expected_epoch {
            return Err(
                "this intent comes from a sidecar generation that is no longer current".to_string(),
            );
        }
        let scope = self
            .known_scopes()
            .into_iter()
            .find(|scope| scope.key == request.scope_key)
            .ok_or_else(|| {
                format!(
                    "{} is not a project this shell has opened",
                    request.scope_key
                )
            })?;
        if !self.is_lifecycle_ready(&scope) {
            return Err(format!(
                "{} is not lifecycle-ready, so no root may be authorized there",
                request.scope_key
            ));
        }
        if request.intent.thread_key.trim().is_empty() {
            return Err("a root intent must name the session it belongs to".to_string());
        }
        if request.intent.workspace.as_os_str().is_empty() {
            return Err("a root intent must name a workspace".to_string());
        }
        let budget = deadline.remaining();
        if budget.is_zero() {
            return Err("no budget remains to authorize this root".to_string());
        }
        let backend = Arc::clone(&self.backend);
        let intent = request.intent.clone();
        let thread_key = intent.thread_key.clone();
        let authorization = run_bounded(budget, move || {
            backend.authorize_root(&scope, &intent, budget)
        })
        .await
        .map_err(|error| error.to_string())?;
        Ok(yi_agent_lifecycle::production::RootAuthorityEntry {
            key: yi_agent_lifecycle::production::RootAuthorityKey {
                scope_key: authorization.scope_key.clone(),
                generation: authorization.daemon_instance_id.clone(),
                client_run_id: authorization.client_run_id.clone(),
                thread_key,
                root_id: authorization.root_id.clone(),
            },
            capability: yi_agent_lifecycle::foreground::SecretCapability::new(
                authorization.capability,
            ),
        })
    }

    /// Releases one project within `deadline`.
    ///
    /// The project is deregistered even when the release fails: the alternative
    /// is re-asking a runtime that already refused, and the failure is reported
    /// rather than retried.
    pub async fn release_project(
        &self,
        scope_key: &str,
        deadline: Deadline,
    ) -> io::Result<ReclaimResult> {
        let scope = {
            let mut guard = self.projects.lock().map_err(|_| poisoned())?;
            guard.remove(scope_key)
        };
        let Some(scope) = scope else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("project {scope_key} is not registered"),
            ));
        };

        let backend = Arc::clone(&self.backend);
        let id = self.client_run_id.clone();
        let budget = deadline.remaining();
        run_bounded(budget, move || backend.release(&id, &scope, budget)).await
    }

    /// Releases every registered project in parallel under one total deadline.
    ///
    /// The returned vector has exactly one entry per registered project, so a
    /// caller can log each outcome. Anything that did not observe a stop carries
    /// `stopped: false`: the shell must never report a teardown it did not see.
    pub async fn shutdown(&self, deadline: Deadline) -> Vec<ReclaimResult> {
        self.quitting.store(true, Ordering::SeqCst);
        let projects: Vec<RuntimeScope> = {
            let mut guard = match self.projects.lock() {
                Ok(guard) => guard,
                Err(_) => return Vec::new(),
            };
            let values = guard.values().cloned().collect();
            guard.clear();
            values
        };
        // The leases are released below, which stops the services they were keeping
        // alive. The local demand table would otherwise still claim those plugins are
        // wanted: nothing reads it again after a quit, but a table left naming
        // consumers that no longer exist is exactly the kind of stale state a later
        // "why did this start?" investigation cannot explain.
        for scope in &projects {
            self.forget_plugin_demand(&scope.key);
        }

        let mut set = tokio::task::JoinSet::new();
        for scope in projects {
            let backend = Arc::clone(&self.backend);
            let id = self.client_run_id.clone();
            let remaining = deadline.remaining();
            set.spawn(async move {
                if remaining.is_zero() {
                    return failed(id, &scope, "the quit budget was already exhausted");
                }
                // The blocking call takes the scope by value; the reporting paths
                // below still need it to name the project.
                let scope_for_io = scope.clone();
                // The blocking client gets the *remaining* budget, and the await
                // is capped by the same value. Without the outer cap, a callback
                // that ignored its socket timeouts would hold the quit open
                // forever; with it, the wait is bounded no matter what the daemon
                // does.
                let id_for_io = id.clone();
                let outcome = tokio::time::timeout(
                    remaining,
                    tokio::task::spawn_blocking(move || {
                        backend.release(&id_for_io, &scope_for_io, remaining)
                    }),
                )
                .await;
                match outcome {
                    Ok(Ok(Ok(result))) => result,
                    Ok(Ok(Err(error))) => failed(id, &scope, &error.to_string()),
                    Ok(Err(join)) => failed(id, &scope, &format!("release task failed: {join}")),
                    Err(_) => failed(
                        id,
                        &scope,
                        "the reclaim was not confirmed within the budget",
                    ),
                }
            });
        }

        let mut results = Vec::new();
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok(result) => results.push(result),
                Err(join) => results.push(ReclaimResult {
                    client_run_id: self.client_run_id.clone(),
                    stopped: false,
                    failures: vec![format!("release task failed: {join}")],
                }),
            }
        }
        results.sort_by(|a, b| a.client_run_id.cmp(&b.client_run_id));
        results
    }
}

/// A `starting` view: nothing is held yet, and waiting is the right move.
fn starting_view(scope_key: &str, attempt_id: &str) -> ProjectAcquireView {
    ProjectAcquireView {
        scope_key: scope_key.to_string(),
        attempt_id: attempt_id.to_string(),
        state: AcquireState::Starting,
        reason: None,
        retry: RetryClass::WhenIdle,
    }
}

/// A `blocked` view with the reason the caller can act on.
fn blocked_view(
    scope_key: &str,
    attempt_id: &str,
    reason: impl Into<String>,
    retry: RetryClass,
) -> ProjectAcquireView {
    ProjectAcquireView {
        scope_key: scope_key.to_string(),
        attempt_id: attempt_id.to_string(),
        state: AcquireState::Blocked,
        reason: Some(reason.into()),
        retry,
    }
}

/// An `unknown` view: the answer was lost, so neither outcome may be assumed.
fn unknown_view(
    scope_key: &str,
    attempt_id: &str,
    reason: impl Into<String>,
) -> ProjectAcquireView {
    ProjectAcquireView {
        scope_key: scope_key.to_string(),
        attempt_id: attempt_id.to_string(),
        state: AcquireState::Unknown,
        reason: Some(reason.into()),
        retry: RetryClass::Backoff,
    }
}

/// Whether the observed runtime's control protocol overlaps this build's.
///
/// A version mismatch that still speaks a common protocol may use the existing
/// preview/confirm flow to switch, so it may hold a lease. A mismatch whose
/// protocol window does not overlap cannot even ask for a switch here.
fn protocols_overlap(
    expected: &ArtifactIdentity,
    observed: &yi_agent_lifecycle::types::RuntimeIdentity,
) -> bool {
    let Some(theirs) = observed.business_protocol else {
        return false;
    };
    expected.business_protocol_min <= theirs && theirs <= expected.business_protocol_max
}

fn failed(client_run_id: String, scope: &RuntimeScope, reason: &str) -> ReclaimResult {
    ReclaimResult {
        client_run_id,
        stopped: false,
        failures: vec![format!("{}: {reason}", scope.key)],
    }
}

/// Runs a blocking backend call off the executor, bounded by `budget`.
///
/// The `timeout` is the load-bearing half. `spawn_blocking` cannot be cancelled,
/// so a callback that never returns would otherwise become an unbounded wait;
/// here it merely becomes a leaked thread the process exits with.
async fn run_bounded<T, F>(budget: Duration, call: F) -> io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> io::Result<T> + Send + 'static,
{
    if budget.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "no budget remains for the lifecycle request",
        ));
    }
    match tokio::time::timeout(budget, tokio::task::spawn_blocking(call)).await {
        Ok(Ok(result)) => result,
        Ok(Err(join)) => Err(io::Error::other(format!("lifecycle task failed: {join}"))),
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "the lifecycle request did not finish within the budget",
        )),
    }
}

// ---------------------------------------------------------------------------
// Quit barrier
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QuitPhase {
    Idle = 0,
    Draining = 1,
    Released = 2,
}

/// What the event loop should do with one `ExitRequested`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitDecision {
    /// Hold this request and start the parallel release.
    BeginDrain,
    /// Hold this request: the drain is already running.
    KeepHolding,
    /// Let the app exit; the drain is complete (or was never needed).
    Allow,
}

/// Decides which `ExitRequested` may proceed.
///
/// Tauri delivers `ExitRequested` again for the `app.exit()` the shell issues at
/// the end of the drain, so the phase is the whole point: the first request is
/// held while every project releases, and the later one passes through untouched.
#[derive(Debug)]
pub struct QuitBarrier {
    phase: AtomicU8,
}

impl QuitBarrier {
    pub fn new() -> Self {
        Self {
            phase: AtomicU8::new(QuitPhase::Idle as u8),
        }
    }

    /// Classifies one `ExitRequested` and advances the phase.
    pub fn decide(&self) -> ExitDecision {
        match self.phase.compare_exchange(
            QuitPhase::Idle as u8,
            QuitPhase::Draining as u8,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => ExitDecision::BeginDrain,
            Err(current) if current == QuitPhase::Draining as u8 => ExitDecision::KeepHolding,
            Err(_) => ExitDecision::Allow,
        }
    }

    /// Marks the drain finished; the `app.exit()` that follows must pass through.
    pub fn release(&self) {
        self.phase
            .store(QuitPhase::Released as u8, Ordering::SeqCst);
    }

    pub fn is_released(&self) -> bool {
        self.phase.load(Ordering::SeqCst) == QuitPhase::Released as u8
    }
}

impl Default for QuitBarrier {
    fn default() -> Self {
        Self::new()
    }
}

/// The outcome of one quit, kept for the log and for tests.
#[derive(Debug, Default, Clone)]
pub struct QuitOutcome {
    pub results: Vec<ReclaimResult>,
    /// `true` only when every project observed a stop. The shell must not report
    /// a clean quit otherwise.
    pub clean: bool,
}

impl QuitOutcome {
    pub fn from_results(results: Vec<ReclaimResult>) -> Self {
        let clean = results.iter().all(|result| result.stopped);
        Self { results, clean }
    }
}

/// The action that ends the app once the drain is done.
///
/// A hook rather than a direct `app.exit(0)` for one concrete reason: Tauri's
/// `MockRuntime` implements `request_exit` with `unimplemented!()`, so a test
/// that ran the real exit would panic before it could assert anything. The hook
/// also keeps "what ends the process" in one visible place.
/// The final exit, replaceable so a test can observe it without a real
/// `AppHandle`. Public because it appears in a public builder signature.
pub type ExitHook = Arc<dyn Fn() + Send + Sync>;

/// Owned by the Tauri app so the run-event handler and the drain task can both
/// reach it. Every field is an `Arc` so the spawned task owns handles rather than
/// borrowing `State`.
pub struct QuitState {
    pub manager: Arc<DesktopRuntimeManager>,
    pub barrier: Arc<QuitBarrier>,
    pub outcome: Arc<Mutex<QuitOutcome>>,
    exit_hook: Option<ExitHook>,
}

impl QuitState {
    pub fn new(manager: Arc<DesktopRuntimeManager>) -> Self {
        Self {
            manager,
            barrier: Arc::new(QuitBarrier::new()),
            outcome: Arc::new(Mutex::new(QuitOutcome::default())),
            exit_hook: None,
        }
    }

    /// Replaces the final exit with `hook`. Only tests need this.
    pub fn with_exit_hook(mut self, hook: ExitHook) -> Self {
        self.exit_hook = Some(hook);
        self
    }

    pub fn outcome(&self) -> QuitOutcome {
        self.outcome
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }
}

/// Installs the lifecycle state for this App run.
///
/// The client run id is generated once, here: every lease taken for the rest of
/// this launch shares it, and an `app-server` restart never changes it.
pub fn install(app: &tauri::AppHandle) {
    use tauri::Manager;

    let id = uuid::Uuid::new_v4().to_string();
    // Verified once, here. `install` runs before any window is shown, so the
    // expensive step (hashing the sidecar) happens before the first status poll and
    // never again.
    let artifact = artifact_identity_for(app);
    let manager = Arc::new(DesktopRuntimeManager::new(id).with_artifact(artifact));
    // The channel looks the session up per preparation, because `install` runs
    // before the sidecar has even spawned.
    let sessions: Arc<dyn ManagementSessions> =
        Arc::new(crate::bridge::LocalSessions::new(app.clone()));
    manager.attach_prepare(Arc::new(
        LocalPrepareChannel::new(Arc::clone(&sessions)).with_manager(Arc::downgrade(&manager)),
    ));
    manager.attach_migration(Arc::new(
        LocalMigrationChannel::new(sessions).with_manager(Arc::downgrade(&manager)),
    ));
    app.manage(QuitState::new(manager));
}

/// The runtime artifact this App shipped with, in the lifecycle crate's shape.
///
/// `None` whenever the identity could not be established -- a `tauri dev` build has
/// no manifest at all. It is deliberately not derived from the wrapper's version: a
/// version string names a release, and what the classifier compares is bytes.
pub fn artifact_identity_for<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
) -> Option<ArtifactIdentity> {
    use tauri::Manager;

    // Tauri reports `<Foo.app>/Contents/Resources` on macOS, not `<Foo.app>`;
    // the rule for turning one into the other lives beside the manifest so a
    // test can hold it to a real bundle tree.
    let resources = app.path().resource_dir().ok()?;
    let bundle_root = crate::runtime_manifest::bundle_root_from_resources(&resources)?;
    artifact_identity_at(&bundle_root)
}

/// The same resolution, from a bundle root, so a test can drive it with a real
/// directory tree instead of a running app.
pub fn artifact_identity_at(bundle_root: &std::path::Path) -> Option<ArtifactIdentity> {
    match crate::runtime_manifest::verify_bundle(bundle_root) {
        crate::runtime_manifest::RuntimeArtifact::Verified {
            manifest,
            executable,
        } => Some(ArtifactIdentity {
            build_id: manifest.build_id,
            display_version: manifest.display_version,
            executable,
            sha256: manifest.sha256,
            schema_min: manifest.schema_min,
            schema_max: manifest.schema_max,
            handover_version: manifest.handover_version,
            business_protocol_min: manifest.business_protocol_min,
            business_protocol_max: manifest.business_protocol_max,
            capabilities: manifest.capabilities,
        }),
        // An unverified bundle has no target: "restart into bytes we could not
        // check" is worse than having no restart button.
        crate::runtime_manifest::RuntimeArtifact::Unknown { .. } => None,
    }
}

/// Releases the barrier if the drain task never reaches its own `release()`.
///
/// Why this exists: the phase is advanced to `Draining` by `decide()`, and only the
/// spawn body moves it to `Released`. If that task panics -- or is dropped because
/// the runtime is shutting down -- nothing else ever releases it, every later
/// `ExitRequested` is held, and the app becomes impossible to quit, including via a
/// second Cmd+Q. A drop guard cannot be forgotten, and `release()` is an idempotent
/// store, so the normal path still releases explicitly before it asks Tauri to exit.
struct ReleaseOnDrop(Arc<QuitBarrier>);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// The `ExitRequested` half of the quit barrier.
///
/// `CloseRequested` is deliberately absent: the red button hides the window, and
/// hiding is not quitting. Releasing a project on hide would reclaim work the
/// user still has open in another window.
pub fn handle_run_event<R: tauri::Runtime>(app: &tauri::AppHandle<R>, event: &tauri::RunEvent) {
    use tauri::Manager;

    let tauri::RunEvent::ExitRequested { api, .. } = event else {
        return;
    };
    let Some(state) = app.try_state::<QuitState>() else {
        // No lifecycle state (a window-only test): nothing to drain.
        return;
    };

    match state.barrier.decide() {
        ExitDecision::BeginDrain => {
            api.prevent_exit();
            let manager = Arc::clone(&state.manager);
            let barrier = Arc::clone(&state.barrier);
            let outcome_slot = Arc::clone(&state.outcome);
            let exit_hook = state.exit_hook.clone();
            // Stop the supervisor first: a sidecar that respawns during the quit
            // would re-attach a runtime this drain is asking to let go.
            crate::bridge::begin_shutdown(app);
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                // Backstop for the phase: if this body panics or is dropped mid-drain,
                // the guard releases the barrier on the way out.
                let _release_on_drop = ReleaseOnDrop(Arc::clone(&barrier));
                let results = manager.shutdown(Deadline::after(QUIT_GRACE)).await;
                let outcome = QuitOutcome::from_results(results);
                for result in &outcome.results {
                    let status = if result.stopped {
                        "stopped"
                    } else {
                        "not confirmed"
                    };
                    eprintln!(
                        "[quit] reclaim for {}: {status}{}",
                        result.client_run_id,
                        if result.failures.is_empty() {
                            String::new()
                        } else {
                            format!(" ({})", result.failures.join("; "))
                        }
                    );
                }
                if let Ok(mut slot) = outcome_slot.lock() {
                    *slot = outcome;
                }
                barrier.release();
                match exit_hook {
                    // The drain's own exit arrives as a second `ExitRequested`,
                    // which the barrier now allows through.
                    Some(hook) => hook(),
                    None => app.exit(0),
                }
            });
        }
        ExitDecision::KeepHolding => api.prevent_exit(),
        ExitDecision::Allow => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Condvar;

    /// A control backend whose releases arrive immediately but return only when
    /// the test says so, which is what makes "parallel under one deadline"
    /// observable instead of inferred.
    struct FakeControl {
        inner: Mutex<FakeInner>,
        gate: Condvar,
        arrived: tokio::sync::Notify,
    }

    #[derive(Default)]
    struct FakeInner {
        released: Vec<String>,
        open: bool,
        refuse: bool,
    }

    impl FakeControl {
        fn new(_root: PathBuf) -> Arc<Self> {
            Arc::new(Self {
                inner: Mutex::new(FakeInner::default()),
                gate: Condvar::new(),
                arrived: tokio::sync::Notify::new(),
            })
        }

        fn arrive(&self, scope: &RuntimeScope) {
            {
                let mut inner = self.inner.lock().unwrap();
                inner.released.push(scope.key.clone());
            }
            self.arrived.notify_waiters();
        }

        fn block_until_open(&self) {
            let mut inner = self.inner.lock().unwrap();
            while !inner.open {
                inner = self.gate.wait(inner).unwrap();
            }
        }

        async fn await_all_releases(&self, expected: usize) {
            loop {
                // Register before checking so an arrival between the check and the
                // await cannot be lost.
                let notified = self.arrived.notified();
                let arrived = self.inner.lock().unwrap().released.len();
                if arrived >= expected {
                    return;
                }
                notified.await;
            }
        }

        fn finish_all(&self) {
            let mut inner = self.inner.lock().unwrap();
            inner.open = true;
            self.gate.notify_all();
        }

        fn released_keys(&self) -> Vec<String> {
            self.inner.lock().unwrap().released.clone()
        }
    }

    impl ControlBackend for FakeControl {
        fn register(
            &self,
            _client_run_id: &str,
            _scope: &RuntimeScope,
            _mode: LeaseMode,
            _budget: Duration,
        ) -> io::Result<bool> {
            Ok(true)
        }

        fn start_heartbeat(&self, _scope: &RuntimeScope) -> io::Result<()> {
            Ok(())
        }

        fn release(
            &self,
            client_run_id: &str,
            scope: &RuntimeScope,
            _budget: Duration,
        ) -> io::Result<ReclaimResult> {
            self.arrive(scope);
            self.block_until_open();
            if self.inner.lock().unwrap().refuse {
                return Err(io::Error::other("the daemon refused the release"));
            }
            Ok(ReclaimResult {
                client_run_id: client_run_id.to_string(),
                stopped: true,
                failures: Vec::new(),
            })
        }
    }

    /// A control backend that records what reached it and answers with fixed
    /// views, so the shell's command surface can be tested without a socket.
    struct RecordingControl {
        inner: Mutex<RecordingInner>,
    }

    #[derive(Default)]
    struct RecordingInner {
        previews: Vec<ArtifactIdentity>,
        confirms: Vec<(String, String)>,
        operations: Vec<String>,
        statuses: usize,
        /// When set, `confirm` fails after recording -- the "answer was lost" case.
        confirm_error: Option<io::ErrorKind>,
    }

    impl RecordingControl {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                inner: Mutex::new(RecordingInner::default()),
            })
        }

        fn inner(&self) -> std::sync::MutexGuard<'_, RecordingInner> {
            self.inner.lock().unwrap()
        }
    }

    impl ControlBackend for RecordingControl {
        fn register(
            &self,
            _client_run_id: &str,
            _scope: &RuntimeScope,
            _mode: LeaseMode,
            _budget: Duration,
        ) -> io::Result<bool> {
            Ok(true)
        }

        fn start_heartbeat(&self, _scope: &RuntimeScope) -> io::Result<()> {
            Ok(())
        }

        fn release(
            &self,
            client_run_id: &str,
            _scope: &RuntimeScope,
            _budget: Duration,
        ) -> io::Result<ReclaimResult> {
            Ok(ReclaimResult {
                client_run_id: client_run_id.to_string(),
                stopped: true,
                failures: Vec::new(),
            })
        }

        fn status(
            &self,
            scope: &RuntimeScope,
            _budget: Duration,
        ) -> io::Result<Option<RuntimeStatusSnapshot>> {
            self.inner().statuses += 1;
            Ok(Some(RuntimeStatusSnapshot {
                identity: yi_agent_lifecycle::types::RuntimeIdentity {
                    scope_key: scope.key.clone(),
                    daemon_instance_id: Some("gen-1".to_string()),
                    guardian_build_id: Some("build-a".to_string()),
                    daemon_build_id: Some("build-a".to_string()),
                    mode: yi_agent_lifecycle::types::RuntimeMode::Managed,
                    business_protocol: Some(1),
                    daemon_display_version: Some("0.2.0".to_string()),
                    guardian_display_version: Some("0.2.0".to_string()),
                    capabilities: vec!["lease.v1".to_string(), "control.v1".to_string()],
                    schema_min: 1,
                    schema_max: 16,
                },
                counts: yi_agent_lifecycle::types::ImpactCounts {
                    clients: 2,
                    pending_clients: 0,
                    running: 1,
                    queued: 0,
                    plugins: 0,
                    complete: false,
                },
                clients: vec![yi_agent_lifecycle::types::ClientSummary {
                    client_run_id: "tui-1".to_string(),
                    kind: ClientKind::Tui,
                    mode: LeaseMode::LeaseOnly,
                }],
                clients_next_cursor: Some("tui-1".to_string()),
                active_operation: None,
                local_restart_available: true,
            }))
        }

        fn preview(
            &self,
            _client_run_id: &str,
            _scope: &RuntimeScope,
            target: ArtifactIdentity,
            _budget: Duration,
        ) -> io::Result<RestartPreview> {
            self.inner().previews.push(target);
            Ok(RestartPreview {
                snapshot_id: "snap-1".to_string(),
                token: "tok-1".to_string(),
                revision: yi_agent_lifecycle::types::ImpactRevision {
                    guardian: 3,
                    coordinator: Some(4),
                },
                counts: yi_agent_lifecycle::types::ImpactCounts {
                    clients: 2,
                    pending_clients: 0,
                    running: 1,
                    queued: 0,
                    plugins: 0,
                    complete: false,
                },
                entries: vec![yi_agent_lifecycle::types::ImpactEntry {
                    kind: "execution".to_string(),
                    id: "run-1".to_string(),
                    owner: Some("tui-1".to_string()),
                }],
                next_cursor: None,
                requires_second_confirmation: true,
                can_restart: true,
                blocked_reason: None,
            })
        }

        fn confirm(
            &self,
            _client_run_id: &str,
            _scope: &RuntimeScope,
            token: &str,
            operation_id: &str,
            _budget: Duration,
        ) -> io::Result<OperationId> {
            let mut inner = self.inner();
            inner
                .confirms
                .push((token.to_string(), operation_id.to_string()));
            if let Some(kind) = inner.confirm_error {
                return Err(io::Error::new(kind, "the answer was lost"));
            }
            Ok(operation_id.to_string())
        }

        fn operation(
            &self,
            _client_run_id: &str,
            _scope: &RuntimeScope,
            operation_id: &str,
            _budget: Duration,
        ) -> io::Result<Option<OperationStatus>> {
            self.inner().operations.push(operation_id.to_string());
            Ok(Some(OperationStatus {
                operation_id: operation_id.to_string(),
                phase: yi_agent_lifecycle::types::SwitchPhase::Draining,
                detail: Some("waiting for clients".to_string()),
                ready_identity: None,
            }))
        }
    }

    fn artifact() -> ArtifactIdentity {
        ArtifactIdentity {
            build_id: "build-b".to_string(),
            display_version: "0.3.0".to_string(),
            executable: PathBuf::from("/Applications/Foo.app/Contents/MacOS/yi-agent"),
            sha256: "bb".to_string(),
            schema_min: 1,
            schema_max: 16,
            handover_version: 1,
            business_protocol_min: 1,
            business_protocol_max: 1,
            capabilities: vec!["lease.v1".to_string(), "control.v1".to_string()],
        }
    }

    /// The preview target is always this shell's own artifact.
    ///
    /// A WebView-supplied path would turn "restart the runtime" into "run this
    /// binary", so the target is taken from the manager, never from the request.
    #[tokio::test]
    async fn preview_always_targets_the_shells_own_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let control = RecordingControl::new();
        let manager = DesktopRuntimeManager::with_backend(
            "app-run".to_string(),
            Arc::clone(&control) as Arc<dyn ControlBackend>,
        )
        .with_artifact(Some(artifact()));
        let project = scope(dir.path(), "alpha");
        manager
            .acquire_project(project.clone(), LeaseMode::Business)
            .await
            .unwrap();

        let preview = manager
            .preview(&project, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(preview.token, "tok-1");
        assert!(preview.requires_second_confirmation);
        assert_eq!(control.inner().previews.len(), 1);
        assert_eq!(control.inner().previews[0].build_id, "build-b");
    }

    /// An unverified shell has no target and says so instead of guessing one.
    #[tokio::test]
    async fn an_unverified_shell_cannot_preview_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let control = RecordingControl::new();
        let manager = DesktopRuntimeManager::with_backend(
            "app-run".to_string(),
            Arc::clone(&control) as Arc<dyn ControlBackend>,
        );
        let project = scope(dir.path(), "alpha");
        manager
            .acquire_project(project.clone(), LeaseMode::Business)
            .await
            .unwrap();

        let error = manager
            .preview(&project, Duration::from_secs(1))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert!(control.inner().previews.is_empty());
    }

    /// The operation id is recorded before the request leaves, so a lost answer is
    /// recoverable by polling instead of by confirming twice.
    #[tokio::test]
    async fn a_lost_confirmation_keeps_its_operation_id_for_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let control = RecordingControl::new();
        control.inner().confirm_error = Some(io::ErrorKind::TimedOut);
        let manager = DesktopRuntimeManager::with_backend(
            "app-run".to_string(),
            Arc::clone(&control) as Arc<dyn ControlBackend>,
        )
        .with_artifact(Some(artifact()));
        let project = scope(dir.path(), "alpha");
        manager
            .acquire_project(project.clone(), LeaseMode::Business)
            .await
            .unwrap();

        // A transport failure is *not* an operation the runtime accepted, so the
        // record must not survive: polling an id nobody knows would look like a
        // restart that vanished.
        let _ = manager
            .confirm(&project, "tok-1", "op-lost", Duration::from_secs(1))
            .await;
        assert_eq!(manager.operation_for(&project.key), None);
        assert_eq!(control.inner().confirms.len(), 1);
    }

    /// A successful confirmation keeps the id, so a reload resumes polling.
    #[tokio::test]
    async fn a_confirmed_operation_is_remembered_for_a_reload() {
        let dir = tempfile::tempdir().unwrap();
        let control = RecordingControl::new();
        let manager = DesktopRuntimeManager::with_backend(
            "app-run".to_string(),
            Arc::clone(&control) as Arc<dyn ControlBackend>,
        )
        .with_artifact(Some(artifact()));
        let project = scope(dir.path(), "alpha");
        manager
            .acquire_project(project.clone(), LeaseMode::Business)
            .await
            .unwrap();

        let id = manager
            .confirm(&project, "tok-1", "op-1", Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(id, "op-1");
        assert_eq!(manager.operation_for(&project.key).as_deref(), Some("op-1"));
        assert_eq!(
            control.inner().confirms,
            vec![("tok-1".to_string(), "op-1".to_string())]
        );

        let status = manager
            .operation(&project, "op-1", Duration::from_secs(1))
            .await
            .unwrap()
            .expect("the operation must answer");
        assert_eq!(
            status.phase,
            yi_agent_lifecycle::types::SwitchPhase::Draining
        );
        // The view renders the phase as its snake-case wire name, the same string
        // the app-server sends, so one component can render both.
        let view = serde_json::to_value(operation_view(&status)).unwrap();
        assert_eq!(view["phase"], "draining");
        assert_eq!(view["operationId"], "op-1");
    }

    /// A status read never registers or renews anything: it must be usable without
    /// touching the lease table at all.
    #[tokio::test]
    async fn status_is_read_only_and_does_not_need_a_lease() {
        let dir = tempfile::tempdir().unwrap();
        let control = RecordingControl::new();
        let manager = DesktopRuntimeManager::with_backend(
            "app-run".to_string(),
            Arc::clone(&control) as Arc<dyn ControlBackend>,
        )
        .with_artifact(Some(artifact()));
        let project = scope(dir.path(), "alpha");

        let snapshot = manager
            .status(&project, Duration::from_secs(1))
            .await
            .unwrap();
        assert!(snapshot.is_some());
        assert_eq!(control.inner().statuses, 1);
        // Nothing was acquired as a side effect.
        assert!(manager.registered_projects().is_empty());

        let view = runtime_view(&manager, &project, snapshot, None).unwrap();
        assert_eq!(view["compatibility"], "compatibleMismatch");
        assert_eq!(view["expectedBuildId"], "build-b");
        assert_eq!(view["counts"]["clients"], 2);
        assert_eq!(view["clients"][0]["kind"], "tui");
        assert_eq!(view["clients"][0]["mode"], "lease_only");
        assert_eq!(view["clientsNextCursor"], "tui-1");
        assert_eq!(view["localRestartAvailable"], true);
        // Two clients and one running task: the banner must be able to say so.
        assert_eq!(view["counts"]["complete"], false);
    }

    /// The same-build case is the one that proves the classifier is wired at all.
    #[tokio::test]
    async fn a_matching_artifact_is_reported_as_the_same_build() {
        let dir = tempfile::tempdir().unwrap();
        let control = RecordingControl::new();
        let mut same = artifact();
        same.build_id = "build-a".to_string();
        let manager = DesktopRuntimeManager::with_backend(
            "app-run".to_string(),
            Arc::clone(&control) as Arc<dyn ControlBackend>,
        )
        .with_artifact(Some(same));
        let project = scope(dir.path(), "alpha");

        let snapshot = manager
            .status(&project, Duration::from_secs(1))
            .await
            .unwrap();
        let view = runtime_view(&manager, &project, snapshot, None).unwrap();
        assert_eq!(view["compatibility"], "sameBuild");
        assert_eq!(view["message"], serde_json::Value::Null);
    }

    /// Nothing answering is `unknown` with a reason, not an error and not a healthy
    /// zero.
    #[test]
    fn no_answer_is_an_unknown_view_with_an_explanation() {
        let dir = tempfile::tempdir().unwrap();
        let manager = DesktopRuntimeManager::with_backend(
            "app-run".to_string(),
            RecordingControl::new() as Arc<dyn ControlBackend>,
        )
        .with_artifact(Some(artifact()));
        let project = scope(dir.path(), "alpha");

        let view = runtime_view(&manager, &project, None, None).unwrap();
        assert_eq!(view["compatibility"], "unknown");
        assert_eq!(view["lifecycleManaged"], false);
        assert_eq!(view["localRestartAvailable"], false);
        assert!(!view["message"].as_str().unwrap().is_empty());
        assert_eq!(view["identity"]["mode"], "unknown");
        assert_eq!(view["counts"]["complete"], false);
    }

    /// A refusal from the shell is a permission-or-state answer, never a transport
    /// error the UI would retry blindly.
    #[test]
    fn an_unregistered_scope_is_refused_rather_than_looked_up() {
        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(DesktopRuntimeManager::with_backend(
            "app-run".to_string(),
            RecordingControl::new() as Arc<dyn ControlBackend>,
        ));
        let state = QuitState::new(manager);
        let error = registered_scope(&state, "/somewhere/else").unwrap_err();
        assert!(error.contains("not open in this app"), "{error}");
        let _ = dir;
    }

    /// Demand is per consumer here because the guardian's rule is "the last holder
    /// releases". If the shell forwarded every view's acquire, two views would make
    /// the guardian believe two clients want one service.
    #[test]
    fn only_the_first_consumer_acquires_and_only_the_last_one_releases() {
        let manager = DesktopRuntimeManager::new("app-run");
        let scope_key = "/p/a/.yi-agent/runtime";

        assert!(
            manager.note_plugin_demand(scope_key, "board", "view-1"),
            "the first consumer is the only one that changes the wanted set"
        );
        assert!(
            !manager.note_plugin_demand(scope_key, "board", "view-2"),
            "a second view joining must not be forwarded as a new acquire"
        );
        assert_eq!(manager.wanted_plugins(scope_key), vec!["board".to_string()]);

        // One view closes: the service must stay, because the other still uses it.
        assert!(
            !manager.drop_plugin_demand(scope_key, "board", "view-1"),
            "another consumer still wants it, so there is nothing to release"
        );
        assert_eq!(manager.wanted_plugins(scope_key), vec!["board".to_string()]);

        assert!(
            manager.drop_plugin_demand(scope_key, "board", "view-2"),
            "the last consumer leaving is what releases the service"
        );
        assert!(manager.wanted_plugins(scope_key).is_empty());
    }

    /// An unbalanced release must not stop a service somebody else is using.
    #[test]
    fn a_consumer_that_never_registered_cannot_release() {
        let manager = DesktopRuntimeManager::new("app-run");
        let scope_key = "/p/a/.yi-agent/runtime";
        assert!(manager.note_plugin_demand(scope_key, "board", "view-1"));
        assert!(
            !manager.drop_plugin_demand(scope_key, "board", "ghost"),
            "an unknown consumer is not the last one"
        );
        assert_eq!(manager.wanted_plugins(scope_key), vec!["board".to_string()]);
    }

    /// Two projects with the same plugin name are two services.
    #[test]
    fn demand_is_scoped_per_project_not_per_plugin_name() {
        let manager = DesktopRuntimeManager::new("app-run");
        assert!(manager.note_plugin_demand("/p/a/.yi-agent/runtime", "board", "a"));
        assert!(manager.note_plugin_demand("/p/b/.yi-agent/runtime", "board", "b"));
        assert_eq!(
            manager.wanted_plugins("/p/a/.yi-agent/runtime"),
            vec!["board".to_string()]
        );
        assert!(manager.drop_plugin_demand("/p/a/.yi-agent/runtime", "board", "a"));
        assert_eq!(
            manager.wanted_plugins("/p/b/.yi-agent/runtime"),
            vec!["board".to_string()],
            "releasing one project's demand must not touch another's"
        );
    }

    // -- legacy migration ---------------------------------------------------

    /// A migration channel whose answers the test names, and which records calls.
    struct FakeMigration {
        calls: Mutex<Vec<(String, serde_json::Value)>>,
        /// One answer per call, in order. `None` means "no answer at all".
        answers: Mutex<Vec<Option<serde_json::Value>>>,
    }

    impl FakeMigration {
        fn answering(answers: Vec<Option<serde_json::Value>>) -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                answers: Mutex::new(answers),
            })
        }

        fn calls(&self) -> Vec<(String, serde_json::Value)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl MigrationChannel for FakeMigration {
        fn call(
            &self,
            method: &str,
            params: serde_json::Value,
            _budget: Duration,
        ) -> io::Result<Option<serde_json::Value>> {
            self.calls
                .lock()
                .unwrap()
                .push((method.to_string(), params));
            let mut answers = self.answers.lock().unwrap();
            if answers.is_empty() {
                return Ok(None);
            }
            Ok(answers.remove(0))
        }
    }

    fn migration_manager(channel: Arc<FakeMigration>) -> DesktopRuntimeManager {
        DesktopRuntimeManager::new("app-run")
            .with_artifact(Some(artifact()))
            .with_migration(channel)
    }

    /// A confirmation names the shell's own artifact, never one the page supplied.
    #[tokio::test]
    async fn a_confirmation_is_sent_with_the_shells_own_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let channel = FakeMigration::answering(vec![Some(serde_json::json!({
            "operation_id": "op-1",
            "phase": "migrating",
            "detail": null,
        }))]);
        let manager = migration_manager(Arc::clone(&channel));

        let view = confirm_migration(&manager, scope(dir.path(), "a"), "tok-1", "op-1", false)
            .await
            .unwrap();
        assert_eq!(
            view.get("phase").and_then(|p| p.as_str()),
            Some("migrating")
        );

        let calls = channel.calls();
        assert_eq!(calls.len(), 1, "one click must send one confirmation");
        assert_eq!(
            calls[0].0,
            crate::runtime_management::MIGRATION_CONFIRM_METHOD
        );
        assert_eq!(
            calls[0].1.get("target").and_then(|t| t.get("build_id")),
            Some(&serde_json::json!("build-b")),
            "the target must be this App's verified artifact"
        );
        assert_eq!(
            calls[0].1.get("operation_id").and_then(|id| id.as_str()),
            Some("op-1")
        );
    }

    /// A confirmation needs a token and an id that existed before the click.
    #[tokio::test]
    async fn a_confirmation_without_a_token_or_an_id_sends_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let channel = FakeMigration::answering(vec![]);
        let manager = migration_manager(Arc::clone(&channel));

        assert!(
            confirm_migration(&manager, scope(dir.path(), "a"), "  ", "op-1", false)
                .await
                .is_err()
        );
        assert!(
            confirm_migration(&manager, scope(dir.path(), "a"), "tok", "", false)
                .await
                .is_err()
        );
        assert!(channel.calls().is_empty(), "{:?}", channel.calls());
    }

    /// An answer lost in flight keeps the id, and says `unknown` rather than failing.
    ///
    /// This is the state that must lead to reconciliation: the migration may be
    /// running, so "failed" would invite a second click.
    #[tokio::test]
    async fn a_lost_confirmation_answer_keeps_the_operation_id() {
        let dir = tempfile::tempdir().unwrap();
        let channel = FakeMigration::answering(vec![None]);
        let manager = migration_manager(Arc::clone(&channel));

        let view = confirm_migration(&manager, scope(dir.path(), "a"), "tok", "op-9", false)
            .await
            .unwrap();
        assert_eq!(view.get("phase").and_then(|p| p.as_str()), Some("unknown"));
        assert_eq!(
            view.get("operationId").and_then(|id| id.as_str()),
            Some("op-9")
        );
        assert_eq!(
            manager.migration_operation_for(&scope(dir.path(), "a").key),
            Some("op-9".to_string()),
            "an unanswered confirmation must stay resumable"
        );
    }

    /// A terminal phase clears the record; a running one does not.
    #[tokio::test]
    async fn only_a_terminal_phase_clears_the_recorded_migration() {
        let dir = tempfile::tempdir().unwrap();
        let channel = FakeMigration::answering(vec![
            Some(serde_json::json!({
                "operation_id": "op-1",
                "phase": "stopping_legacy",
                "detail": null,
            })),
            Some(serde_json::json!({
                "operation_id": "op-1",
                "phase": "completed",
                "detail": null,
            })),
        ]);
        let manager = migration_manager(Arc::clone(&channel));
        let key = scope(dir.path(), "a").key;
        manager.note_migration_operation(&key, "op-1");

        read_migration_operation(&manager, &key, "op-1")
            .await
            .unwrap();
        assert_eq!(
            manager.migration_operation_for(&key),
            Some("op-1".to_string()),
            "a migration still stopping something must stay visible"
        );

        read_migration_operation(&manager, &key, "op-1")
            .await
            .unwrap();
        assert_eq!(manager.migration_operation_for(&key), None);
    }

    /// The terminal set is the shell's contract with the UI, so it is pinned.
    #[test]
    fn terminal_phases_are_exactly_the_ones_that_end_an_operation() {
        for phase in [
            "completed",
            "failed",
            "temporary_use",
            "manual_action_required",
        ] {
            assert!(is_terminal_migration_phase(phase), "{phase}");
        }
        for phase in ["accepted", "stopping_legacy", "starting_target", "unknown"] {
            assert!(!is_terminal_migration_phase(phase), "{phase}");
        }
    }

    /// A build with no verified artifact refuses to migrate rather than guessing one.
    #[tokio::test]
    async fn a_build_without_a_verified_artifact_cannot_migrate() {
        let dir = tempfile::tempdir().unwrap();
        let channel = FakeMigration::answering(vec![]);
        let manager = DesktopRuntimeManager::new("app-run").with_migration(channel.clone());

        let error = confirm_migration(&manager, scope(dir.path(), "a"), "tok", "op-1", false)
            .await
            .unwrap_err();
        assert!(error.contains("no verified runtime artifact"), "{error}");
        assert!(channel.calls().is_empty());
    }

    fn scope(root: &std::path::Path, name: &str) -> RuntimeScope {
        RuntimeScope {
            key: format!("{}/{name}", root.display()),
            runtime_dir: root.join(name).join(".yi-agent/runtime"),
            project_key: root.join(name).display().to_string(),
            instance_home: root.join("home"),
        }
    }

    /// Three projects registered on one real manager, with a fake control channel.
    async fn three_project_manager() -> (Arc<DesktopRuntimeManager>, Arc<FakeControl>) {
        let dir = tempfile::tempdir().unwrap();
        let control = FakeControl::new(dir.path().to_path_buf());
        let manager = Arc::new(DesktopRuntimeManager::with_backend(
            "app-run".to_string(),
            control.clone(),
        ));
        for name in ["alpha", "beta", "gamma"] {
            manager
                .acquire_project(scope(dir.path(), name), LeaseMode::Business)
                .await
                .unwrap();
        }
        // Leak the tempdir for the test's lifetime: the scopes only embed paths.
        std::mem::forget(dir);
        (manager, control)
    }

    /// A deadline on the paused Tokio clock, so advancing time in the test is what
    /// expires it. `Deadline::after` would read the real clock and measure
    /// nothing.
    fn paused_deadline(budget: Duration) -> Deadline {
        Deadline::with_clock(budget, Arc::new(TokioClock::new()))
    }

    #[tokio::test(start_paused = true)]
    async fn multi_project_quit_releases_in_parallel_under_one_deadline() {
        let (manager, control) = three_project_manager().await;

        let task = tokio::spawn(async move {
            manager
                .shutdown(paused_deadline(Duration::from_secs(15)))
                .await
        });

        control.await_all_releases(3).await;
        tokio::time::advance(Duration::from_secs(10)).await;
        control.finish_all();

        let results = task.await.unwrap();
        assert_eq!(results.len(), 3);
        assert!(
            results.iter().all(|result| result.stopped),
            "every project must report a stop: {results:?}"
        );
        assert_eq!(control.released_keys().len(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn a_hung_daemon_cannot_hold_the_quit_past_the_budget() {
        let (manager, control) = three_project_manager().await;

        let task = tokio::spawn(async move {
            manager
                .shutdown(paused_deadline(Duration::from_secs(15)))
                .await
        });

        control.await_all_releases(3).await;
        // Nothing is allowed to finish. Past the budget the manager must return
        // anyway, with one honest failure per project.
        tokio::time::advance(Duration::from_secs(16)).await;
        let results = task.await.unwrap();
        assert_eq!(results.len(), 3);
        assert!(
            results.iter().all(|result| !result.stopped),
            "a reclaim nobody confirmed must never be reported as a stop: {results:?}"
        );
        assert!(results.iter().all(|result| !result.failures.is_empty()));

        // Release the blocked fake threads so the test runtime can shut down.
        control.finish_all();
    }

    #[tokio::test(start_paused = true)]
    async fn one_total_budget_is_not_multiplied_by_the_project_count() {
        let (manager, control) = three_project_manager().await;
        let start = TokioClock::new().now();

        let task = tokio::spawn(async move {
            manager
                .shutdown(paused_deadline(Duration::from_secs(15)))
                .await
        });
        control.await_all_releases(3).await;
        tokio::time::advance(Duration::from_secs(14)).await;
        control.finish_all();
        let results = task.await.unwrap();

        // Three sequential 10-second releases would take 30 s; a single shared
        // budget finishes well inside it.
        assert!(TokioClock::new().now() - start < Duration::from_secs(30));
        assert_eq!(results.len(), 3);
        assert!(results.iter().all(|result| result.stopped));
    }

    #[tokio::test(start_paused = true)]
    async fn a_refused_release_is_reported_as_a_failure_not_a_stop() {
        let dir = tempfile::tempdir().unwrap();
        let control = FakeControl::new(dir.path().to_path_buf());
        control.inner.lock().unwrap().refuse = true;
        let manager = DesktopRuntimeManager::with_backend("app-run", control.clone());
        manager
            .acquire_project(scope(dir.path(), "alpha"), LeaseMode::Business)
            .await
            .unwrap();

        let task = tokio::spawn(async move {
            manager
                .shutdown(paused_deadline(Duration::from_secs(15)))
                .await
        });
        control.await_all_releases(1).await;
        control.finish_all();
        let results = task.await.unwrap();

        assert_eq!(results.len(), 1);
        assert!(!results[0].stopped);
        assert!(results[0].failures[0].contains("refused"));
        std::mem::forget(dir);
    }

    /// The fake clock must move with the runtime, or the deadline tests above
    /// would be meaningless.
    #[tokio::test(start_paused = true)]
    async fn the_test_clock_follows_the_tokio_clock() {
        let clock = TokioClock::new();
        assert_eq!(clock.now(), Duration::ZERO);
        tokio::time::advance(Duration::from_secs(3)).await;
        assert_eq!(clock.now(), Duration::from_secs(3));
    }

    #[tokio::test]
    async fn a_second_acquire_of_the_same_project_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let control = FakeControl::new(dir.path().to_path_buf());
        let manager = DesktopRuntimeManager::with_backend("app-run", control);
        let project = scope(dir.path(), "alpha");

        manager
            .acquire_project(project.clone(), LeaseMode::Business)
            .await
            .unwrap();
        manager
            .acquire_project(project.clone(), LeaseMode::Business)
            .await
            .unwrap();
        assert_eq!(manager.registered_projects().len(), 1);
        std::mem::forget(dir);
    }

    #[tokio::test]
    async fn a_project_is_never_registered_after_the_quit_has_begun() {
        let dir = tempfile::tempdir().unwrap();
        let control = FakeControl::new(dir.path().to_path_buf());
        control.inner.lock().unwrap().open = true;
        let manager = DesktopRuntimeManager::with_backend("app-run", control);
        manager
            .shutdown(paused_deadline(Duration::from_secs(1)))
            .await;

        let error = manager
            .acquire_project(scope(dir.path(), "alpha"), LeaseMode::Business)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("quitting"));
        std::mem::forget(dir);
    }

    #[tokio::test]
    async fn releasing_an_unknown_project_is_an_error_not_a_silent_success() {
        let dir = tempfile::tempdir().unwrap();
        let control = FakeControl::new(dir.path().to_path_buf());
        let manager = DesktopRuntimeManager::with_backend("app-run", control);
        let error = manager
            .release_project("does-not-exist", paused_deadline(Duration::from_secs(1)))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        std::mem::forget(dir);
    }

    #[test]
    fn the_barrier_holds_once_then_allows_the_final_exit() {
        let barrier = QuitBarrier::new();
        assert_eq!(barrier.decide(), ExitDecision::BeginDrain);
        // A second request while the drain runs must not start a second drain.
        assert_eq!(barrier.decide(), ExitDecision::KeepHolding);
        barrier.release();
        // The `app.exit(0)` issued by the drain task arrives here.
        assert_eq!(barrier.decide(), ExitDecision::Allow);
        assert!(barrier.is_released());
    }

    /// A drain task that dies without releasing must not wedge the quit.
    ///
    /// The phase moves to `Draining` in `decide()`, and reaching `Released` is the
    /// spawned body's job. If that body panics or is dropped, the guard is the only
    /// thing standing between the user and an app that answers every later Cmd+Q
    /// with `KeepHolding` -- i.e. one that cannot be quit at all.
    #[test]
    fn a_drain_that_dies_still_leaves_the_app_quittable() {
        let barrier = Arc::new(QuitBarrier::new());
        assert_eq!(barrier.decide(), ExitDecision::BeginDrain);
        assert_eq!(barrier.decide(), ExitDecision::KeepHolding);

        {
            let _guard = ReleaseOnDrop(Arc::clone(&barrier));
            // The drain body never gets to run its own `release()`.
        }

        // The next Cmd+Q passes instead of being held forever.
        assert_eq!(barrier.decide(), ExitDecision::Allow);
        assert!(barrier.is_released());
    }

    /// A scope with no guardian has no lease to hold. Recording it anyway would
    /// make every later quit report a reclaim failure for work that never had an
    /// owner -- a false failure, which is just as bad as a false success.
    #[tokio::test]
    async fn a_scope_without_a_guardian_is_not_registered_for_the_quit() {
        struct NoGuardian;

        impl ControlBackend for NoGuardian {
            fn register(
                &self,
                _client_run_id: &str,
                _scope: &RuntimeScope,
                _mode: LeaseMode,
                _budget: Duration,
            ) -> io::Result<bool> {
                Ok(false)
            }

            fn start_heartbeat(&self, _scope: &RuntimeScope) -> io::Result<()> {
                Ok(())
            }

            fn release(
                &self,
                client_run_id: &str,
                scope: &RuntimeScope,
                _budget: Duration,
            ) -> io::Result<ReclaimResult> {
                Ok(failed(client_run_id.to_string(), scope, "should never run"))
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let manager =
            DesktopRuntimeManager::with_backend("app-run".to_string(), Arc::new(NoGuardian));
        manager
            .acquire_project(scope(dir.path(), "alpha"), LeaseMode::Business)
            .await
            .expect("an unmanaged scope is not an error");

        assert!(manager.registered_projects().is_empty());
        assert!(manager
            .shutdown(Deadline::after(Duration::from_secs(1)))
            .await
            .is_empty());
    }

    #[test]
    fn a_clean_quit_requires_every_project_to_report_a_stop() {
        let clean = QuitOutcome::from_results(vec![ReclaimResult {
            client_run_id: "a".into(),
            stopped: true,
            failures: Vec::new(),
        }]);
        assert!(clean.clean);

        let unclean = QuitOutcome::from_results(vec![ReclaimResult {
            client_run_id: "a".into(),
            stopped: false,
            failures: vec!["timeout".into()],
        }]);
        assert!(!unclean.clean);
    }

    #[test]
    fn the_socket_backend_uses_the_guardians_socket_name() {
        let scope = RuntimeScope {
            key: "/r".into(),
            runtime_dir: PathBuf::from("/r"),
            project_key: "/p".into(),
            instance_home: PathBuf::from("/h"),
        };
        assert_eq!(
            SocketBackend::control_path(&scope),
            PathBuf::from("/r").join(GUARDIAN_SOCKET_FILE)
        );
    }

    #[test]
    fn the_lifecycle_crate_never_brings_sqlite_into_the_shell() {
        // Structural guard: the shell links `yi-agent-lifecycle` directly, so a
        // dependency added there would silently give the Tauri binary a second
        // database stack. Only dependency *declarations* count -- the manifest's
        // comments deliberately mention `rusqlite` while explaining the ban.
        let manifest = include_str!("../../../yi-agent-rs/crates/yi-agent-lifecycle/Cargo.toml");
        let declarations: Vec<&str> = manifest
            .lines()
            .map(str::trim)
            .filter(|line| !line.starts_with('#') && !line.is_empty())
            .collect();

        for banned in [
            "rusqlite",
            "yi-agent-store",
            "yi-agent-core",
            "libsqlite3-sys",
        ] {
            assert!(
                !declarations.iter().any(|line| line.contains(banned)),
                "yi-agent-lifecycle must not depend on {banned}: the Tauri shell links it directly"
            );
        }

        // And the promise only holds while the md5-style escape hatches stay out:
        // a `[dependencies.foo]` table would not be caught by the line scan above.
        assert!(!manifest.contains("[dependencies."));
    }

    /// Counts how many releases ran concurrently, to prove "parallel" is real and
    /// not an artifact of the fake returning instantly.
    struct PeakCounter {
        live: AtomicUsize,
        peak: AtomicUsize,
    }

    impl ControlBackend for PeakCounter {
        fn register(
            &self,
            _client_run_id: &str,
            _scope: &RuntimeScope,
            _mode: LeaseMode,
            _budget: Duration,
        ) -> io::Result<bool> {
            Ok(true)
        }

        fn start_heartbeat(&self, _scope: &RuntimeScope) -> io::Result<()> {
            Ok(())
        }

        fn release(
            &self,
            client_run_id: &str,
            _scope: &RuntimeScope,
            _budget: Duration,
        ) -> io::Result<ReclaimResult> {
            let live = self.live.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(live, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(20));
            self.live.fetch_sub(1, Ordering::SeqCst);
            Ok(ReclaimResult {
                client_run_id: client_run_id.to_string(),
                stopped: true,
                failures: Vec::new(),
            })
        }
    }

    #[tokio::test]
    async fn the_releases_really_run_at_the_same_time() {
        let dir = tempfile::tempdir().unwrap();
        let counter = Arc::new(PeakCounter {
            live: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        });
        let manager = DesktopRuntimeManager::with_backend(
            "app-run",
            Arc::clone(&counter) as Arc<dyn ControlBackend>,
        );
        for name in ["alpha", "beta", "gamma"] {
            manager
                .acquire_project(scope(dir.path(), name), LeaseMode::Business)
                .await
                .unwrap();
        }
        let results = manager
            .shutdown(Deadline::after(Duration::from_secs(10)))
            .await;
        assert_eq!(results.len(), 3);
        assert!(
            counter.peak.load(Ordering::SeqCst) > 1,
            "releases ran one at a time"
        );
        std::mem::forget(dir);
    }

    // -----------------------------------------------------------------------
    // Project acquisition: prepare -> register -> heartbeat -> ready
    // -----------------------------------------------------------------------

    /// Knobs a test flips to make the fake refuse at a chosen step.
    struct Knobs {
        served: bool,
        heartbeat_fails: bool,
        generation: Option<&'static str>,
        refuse: Vec<String>,
        /// When true, `register` blocks until `release_register` is called.
        hold_register: bool,
        register_released: bool,
    }

    impl Default for Knobs {
        fn default() -> Self {
            Self {
                served: true,
                heartbeat_fails: false,
                generation: Some("gen-1"),
                refuse: Vec::new(),
                hold_register: false,
                register_released: false,
            }
        }
    }

    /// A control backend that records the order of the calls it sees.
    ///
    /// The order *is* the property under test. Registering before preparation
    /// would take a lease on a runtime nobody identified; renewing before
    /// registering would renew a grant that does not exist.
    struct TracingControl {
        trace: Arc<Mutex<Vec<&'static str>>>,
        knobs: Mutex<Knobs>,
    }

    impl TracingControl {
        fn record(&self, label: &'static str) {
            self.trace.lock().unwrap().push(label);
        }
    }

    impl ControlBackend for TracingControl {
        fn register(
            &self,
            _client_run_id: &str,
            scope: &RuntimeScope,
            _mode: LeaseMode,
            _budget: Duration,
        ) -> io::Result<bool> {
            self.record("register_business");
            // A test can park the registration here to hold it in flight; the
            // production path never sets `hold_register`.
            loop {
                {
                    let knobs = self.knobs.lock().unwrap();
                    if !knobs.hold_register || knobs.register_released {
                        break;
                    }
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            let knobs = self.knobs.lock().unwrap();
            if knobs.refuse.iter().any(|key| key == &scope.key) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "the guardian refused this client",
                ));
            }
            Ok(knobs.served)
        }

        fn start_heartbeat(&self, _scope: &RuntimeScope) -> io::Result<()> {
            self.record("start_heartbeat");
            if self.knobs.lock().unwrap().heartbeat_fails {
                return Err(io::Error::other("the renewal thread could not start"));
            }
            Ok(())
        }

        fn lease_mode(&self, _scope: &RuntimeScope) -> Option<LeaseMode> {
            Some(LeaseMode::Business)
        }

        fn lease_is_live(&self, _scope: &RuntimeScope) -> bool {
            let knobs = self.knobs.lock().unwrap();
            knobs.served && !knobs.heartbeat_fails
        }

        fn release(
            &self,
            client_run_id: &str,
            _scope: &RuntimeScope,
            _budget: Duration,
        ) -> io::Result<ReclaimResult> {
            self.record("release");
            Ok(ReclaimResult {
                client_run_id: client_run_id.to_string(),
                stopped: true,
                failures: Vec::new(),
            })
        }

        fn status(
            &self,
            scope: &RuntimeScope,
            _budget: Duration,
        ) -> io::Result<Option<RuntimeStatusSnapshot>> {
            self.record("status");
            let generation = self.knobs.lock().unwrap().generation;
            Ok(Some(status_snapshot(scope, generation)))
        }
    }

    fn status_snapshot(scope: &RuntimeScope, generation: Option<&str>) -> RuntimeStatusSnapshot {
        status_snapshot_for(&scope.key, generation)
    }

    fn status_snapshot_for(scope_key: &str, generation: Option<&str>) -> RuntimeStatusSnapshot {
        RuntimeStatusSnapshot {
            identity: yi_agent_lifecycle::types::RuntimeIdentity {
                scope_key: scope_key.to_string(),
                daemon_instance_id: generation.map(str::to_string),
                guardian_build_id: Some("build-b".to_string()),
                daemon_build_id: Some("build-b".to_string()),
                mode: yi_agent_lifecycle::types::RuntimeMode::Managed,
                business_protocol: Some(1),
                daemon_display_version: Some("0.3.0".to_string()),
                guardian_display_version: Some("0.3.0".to_string()),
                capabilities: vec!["lease.v1".to_string(), "control.v1".to_string()],
                schema_min: 1,
                schema_max: 16,
            },
            counts: yi_agent_lifecycle::types::ImpactCounts {
                clients: 1,
                pending_clients: 0,
                running: 0,
                queued: 0,
                plugins: 0,
                complete: false,
            },
            clients: Vec::new(),
            clients_next_cursor: None,
            active_operation: None,
            local_restart_available: true,
        }
    }

    /// How the fake preparation channel should answer.
    enum Reply {
        /// The same outcome for every request.
        Fixed(PrepareOutcome),
        /// `Connected` with the request's own scope, as a real runtime would.
        ConnectedToRequest,
    }

    /// A preparation channel that records the call and answers as told.
    struct FakePrepare {
        reply: Reply,
        trace: Arc<Mutex<Vec<&'static str>>>,
    }

    impl PrepareChannel for FakePrepare {
        fn prepare(
            &self,
            request: PrepareRequest,
            _budget: Duration,
        ) -> io::Result<PrepareOutcome> {
            self.trace.lock().unwrap().push("prepare");
            Ok(match &self.reply {
                Reply::Fixed(outcome) => outcome.clone(),
                Reply::ConnectedToRequest => {
                    PrepareOutcome::Connected(managed_identity(&request.scope.key, "gen-1"))
                }
            })
        }
    }

    fn managed_identity(
        scope_key: &str,
        generation: &str,
    ) -> yi_agent_lifecycle::types::RuntimeIdentity {
        yi_agent_lifecycle::types::RuntimeIdentity {
            scope_key: scope_key.to_string(),
            daemon_instance_id: Some(generation.to_string()),
            guardian_build_id: Some("build-b".to_string()),
            daemon_build_id: Some("build-b".to_string()),
            mode: yi_agent_lifecycle::types::RuntimeMode::Managed,
            business_protocol: Some(1),
            daemon_display_version: Some("0.3.0".to_string()),
            guardian_display_version: Some("0.3.0".to_string()),
            capabilities: vec!["lease.v1".to_string(), "control.v1".to_string()],
            schema_min: 1,
            schema_max: 16,
        }
    }

    /// Everything a Task 3 test needs: the manager, the fake, and the shared trace.
    struct Wired {
        manager: Arc<DesktopRuntimeManager>,
        control: Arc<TracingControl>,
        trace: Arc<Mutex<Vec<&'static str>>>,
    }

    impl Wired {
        fn new(outcome: PrepareOutcome) -> Self {
            Self::with_reply(Reply::Fixed(outcome))
        }

        fn connected() -> Self {
            Self::with_reply(Reply::ConnectedToRequest)
        }

        fn with_reply(reply: Reply) -> Self {
            let trace = Arc::new(Mutex::new(Vec::new()));
            let control = Arc::new(TracingControl {
                trace: Arc::clone(&trace),
                knobs: Mutex::new(Knobs::default()),
            });
            let prepare = Arc::new(FakePrepare {
                reply,
                trace: Arc::clone(&trace),
            });
            let manager = DesktopRuntimeManager::with_backend(
                "app-run",
                Arc::clone(&control) as Arc<dyn ControlBackend>,
            )
            .with_prepare(prepare)
            .with_artifact(Some(artifact()));
            Self {
                manager: Arc::new(manager),
                control,
                trace,
            }
        }

        fn trace(&self) -> Vec<&'static str> {
            self.trace.lock().unwrap().clone()
        }
    }

    #[tokio::test]
    async fn a_connected_runtime_is_prepared_registered_then_renewed() {
        let dir = tempfile::tempdir().unwrap();
        let project = scope(dir.path(), "alpha");
        let wired = Wired::new(PrepareOutcome::Connected(managed_identity(
            &project.key,
            "gen-1",
        )));
        let view = wired
            .manager
            .ensure_project(&project, Duration::from_secs(2))
            .await
            .unwrap();

        assert_eq!(
            wired.trace(),
            ["prepare", "register_business", "start_heartbeat", "status"],
            "the lease may only be taken after preparation identified the runtime, \
             and renewal may only start once the lease exists"
        );
        assert_eq!(view.state, AcquireState::ManagedReady);
        assert_eq!(view.scope_key, project.key);
        assert!(!view.attempt_id.is_empty());
        // Ready is lifecycle-ready, and the quit has something to release.
        assert!(wired.manager.is_lifecycle_ready(&project));
        assert_eq!(wired.manager.registered_projects(), vec![project.clone()]);
        std::mem::forget(dir);
    }

    /// A project the runtime does not serve holds nothing and claims nothing.
    ///
    /// `Ok(false)` is the honest answer from a backend with no guardian. Treating
    /// it as a lease would make every exit report a reclaim failure that never
    /// happened, and would let ready describe a runtime nobody manages.
    #[tokio::test]
    async fn no_lease_never_becomes_ready_or_owned() {
        let dir = tempfile::tempdir().unwrap();
        let project = scope(dir.path(), "alpha");
        let wired = Wired::new(PrepareOutcome::Connected(managed_identity(
            &project.key,
            "gen-1",
        )));
        wired.control.knobs.lock().unwrap().served = false;

        let view = wired
            .manager
            .ensure_project(&project, Duration::from_secs(2))
            .await
            .unwrap();

        assert_ne!(view.state, AcquireState::ManagedReady);
        assert_eq!(view.state, AcquireState::Unknown);
        assert!(!wired.manager.is_lifecycle_ready(&project));
        assert!(
            wired.manager.registered_projects().is_empty(),
            "a scope with no lease must not appear in the quit's work list"
        );
        assert!(
            !wired.trace().contains(&"start_heartbeat"),
            "there is nothing to renew: {:?}",
            wired.trace()
        );
        std::mem::forget(dir);
    }

    /// A renewal that cannot start leaves no usable ready view *and* no lease.
    ///
    /// A lease whose heartbeat never started looks held until the guardian expires
    /// it, which is exactly the window where the shell would wrongly let work run.
    #[tokio::test]
    async fn a_failed_heartbeat_leaves_no_ready_view_and_releases_the_lease() {
        let dir = tempfile::tempdir().unwrap();
        let project = scope(dir.path(), "alpha");
        let wired = Wired::new(PrepareOutcome::Connected(managed_identity(
            &project.key,
            "gen-1",
        )));
        wired.control.knobs.lock().unwrap().heartbeat_fails = true;

        let view = wired
            .manager
            .ensure_project(&project, Duration::from_secs(2))
            .await
            .unwrap();

        assert_eq!(view.state, AcquireState::Blocked);
        assert!(view
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("renewed")));
        assert!(!wired.manager.is_lifecycle_ready(&project));
        assert!(wired.manager.registered_projects().is_empty());
        assert!(
            wired.trace().contains(&"release"),
            "the abandoned lease must be released, not left to expire: {:?}",
            wired.trace()
        );
        std::mem::forget(dir);
    }

    /// One blocked project must not block another: each scope has its own attempt.
    #[tokio::test]
    async fn a_blocked_scope_does_not_block_a_healthy_one() {
        let dir = tempfile::tempdir().unwrap();
        let alpha = scope(dir.path(), "alpha");
        let beta = scope(dir.path(), "beta");
        let wired = Wired::connected();
        wired.control.knobs.lock().unwrap().refuse = vec![alpha.key.clone()];
        let manager = &wired.manager;

        let alpha_view = manager
            .ensure_project(&alpha, Duration::from_secs(2))
            .await
            .unwrap();
        let beta_view = manager
            .ensure_project(&beta, Duration::from_secs(2))
            .await
            .unwrap();

        assert_eq!(alpha_view.state, AcquireState::Blocked);
        assert!(
            beta_view.state != AcquireState::Blocked,
            "one project's refusal must not become another's: {beta_view:?}"
        );
        assert_eq!(manager.registered_projects(), vec![beta]);
        std::mem::forget(dir);
    }

    /// A compatible build mismatch may hold a lease so the existing preview/confirm
    /// flow can offer a switch, but it is never ready: no root, no business, no
    /// plugin demand.
    #[tokio::test]
    async fn a_compatible_version_mismatch_holds_a_lease_without_becoming_ready() {
        let dir = tempfile::tempdir().unwrap();
        let project = scope(dir.path(), "alpha");
        let wired = Wired::connected();
        let view = wired
            .manager
            .ensure_project(&project, Duration::from_secs(2))
            .await
            .unwrap();

        assert_eq!(view.state, AcquireState::ManagedReady);
        // Now the same project, but the runtime in front of it is another build.
        let mismatch = Wired::with_reply(Reply::Fixed(PrepareOutcome::VersionChoiceRequired(
            managed_identity(&project.key, "gen-1"),
        )));
        let view = mismatch
            .manager
            .ensure_project(&project, Duration::from_secs(2))
            .await
            .unwrap();

        assert_eq!(view.state, AcquireState::Blocked);
        assert_eq!(view.retry, RetryClass::UserAction);
        // The lease is held (the mismatch is protocol-compatible)...
        let state = mismatch.manager.project_state(&project.key).unwrap();
        assert!(
            state.held,
            "a compatible mismatch may use its lease to switch"
        );
        // ...and it is being renewed, because a person deciding may take longer
        // than the lease window. A held-but-unrenewed lease would expire under the
        // very preview the user is reading.
        assert!(
            mismatch.trace.lock().unwrap().contains(&"start_heartbeat"),
            "a held lease must be renewed from the moment it is taken: {:?}",
            mismatch.trace.lock().unwrap()
        );
        // ...and preview/confirm work off that held lease...
        assert_eq!(
            mismatch.manager.registered_projects(),
            vec![project.clone()]
        );
        // ...while nothing that runs work is allowed.
        assert!(!mismatch.manager.is_lifecycle_ready(&project));
        std::mem::forget(dir);
    }

    /// A mismatch whose control protocol does not overlap cannot even hold a lease.
    #[tokio::test]
    async fn an_incompatible_version_mismatch_holds_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let project = scope(dir.path(), "alpha");
        let mut identity = managed_identity(&project.key, "gen-1");
        identity.business_protocol = Some(99);
        let wired = Wired::new(PrepareOutcome::VersionChoiceRequired(identity));

        let view = wired
            .manager
            .ensure_project(&project, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(view.state, AcquireState::Blocked);
        assert!(wired.manager.registered_projects().is_empty());
        assert!(
            !wired.trace().contains(&"register_business"),
            "an incompatible runtime must not be registered against: {:?}",
            wired.trace()
        );
        std::mem::forget(dir);
    }

    /// An unmanaged runtime is a user decision, never something to adopt silently.
    #[tokio::test]
    async fn an_unmanaged_runtime_asks_the_user_instead_of_adopting_it() {
        let dir = tempfile::tempdir().unwrap();
        let project = scope(dir.path(), "alpha");
        let wired = Wired::new(PrepareOutcome::LegacyChoiceRequired {
            reason: "a manually started daemon serves this project".to_string(),
        });
        let view = wired
            .manager
            .ensure_project(&project, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(view.state, AcquireState::LegacyChoiceRequired);
        assert_eq!(view.retry, RetryClass::UserAction);
        assert!(wired.manager.registered_projects().is_empty());
        std::mem::forget(dir);
    }

    /// Ready is only claimed for the generation preparation actually observed.
    ///
    /// A generation change between the answer and the check means a different
    /// runtime now serves the scope, and ready must not describe it.
    #[tokio::test]
    async fn a_generation_change_after_the_lease_prevents_ready() {
        let dir = tempfile::tempdir().unwrap();
        let project = scope(dir.path(), "alpha");
        let wired = Wired::new(PrepareOutcome::Connected(managed_identity(
            &project.key,
            "gen-1",
        )));
        wired.control.knobs.lock().unwrap().generation = Some("gen-2");

        let view = wired
            .manager
            .ensure_project(&project, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(view.state, AcquireState::Unknown);
        assert!(!wired.manager.is_lifecycle_ready(&project));
        std::mem::forget(dir);
    }

    /// A scope the shell never opened cannot be authorized, whatever the intent says.
    #[tokio::test]
    async fn an_intent_for_an_unopened_scope_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let project = scope(dir.path(), "alpha");
        let wired = Wired::connected();
        wired.manager.note_sidecar_epoch("epoch-1");
        let view = wired
            .manager
            .ensure_project(&project, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(view.state, AcquireState::ManagedReady);

        // A scope the shell has neither opened nor leased. Being leased counts as
        // "known" by design (the quit and the status banner both need it), so the
        // negative case must use a key the shell never resolved.
        let error = wired
            .manager
            .authorize_intent(root_intent("/no/such/scope", "thread:a"), once_deadline())
            .await
            .unwrap_err();
        assert!(
            error.contains("not a project this shell has opened"),
            "unexpected error: {error}"
        );
        std::mem::forget(dir);
    }

    /// A ready project still refuses a root intent from a stale sidecar generation.
    #[tokio::test]
    async fn an_intent_from_a_stale_sidecar_epoch_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let project = scope(dir.path(), "alpha");
        let wired = Wired::connected();
        wired.manager.note_open(&project);
        wired.manager.note_sidecar_epoch("epoch-2");
        wired
            .manager
            .ensure_project(&project, Duration::from_secs(2))
            .await
            .unwrap();

        let mut intent = root_intent(&project.key, "thread:a");
        intent.sidecar_epoch = "epoch-1".to_string();
        let error = wired
            .manager
            .authorize_intent(intent, once_deadline())
            .await
            .unwrap_err();
        assert!(error.contains("no longer current"), "unexpected: {error}");

        // The current epoch passes the epoch gate: it reaches the backend, which
        // here has no `authorize_root` and therefore says so. This is what makes
        // the assertion above about the *epoch*, not about any refusal.
        let mut current = root_intent(&project.key, "thread:a");
        current.sidecar_epoch = "epoch-2".to_string();
        let error = wired
            .manager
            .authorize_intent(current, once_deadline())
            .await
            .unwrap_err();
        assert!(
            error.contains("cannot authorize a root"),
            "the current epoch must get past the gate: {error}"
        );
        std::mem::forget(dir);
    }

    /// A project that never became lifecycle-ready cannot authorize a root, even
    /// while its lease is still held.
    #[tokio::test]
    async fn a_held_but_blocked_project_cannot_authorize_a_root() {
        let dir = tempfile::tempdir().unwrap();
        let project = scope(dir.path(), "alpha");
        let wired = Wired::new(PrepareOutcome::VersionChoiceRequired(managed_identity(
            &project.key,
            "gen-1",
        )));
        wired.manager.note_open(&project);
        wired.manager.note_sidecar_epoch("epoch-1");
        wired
            .manager
            .ensure_project(&project, Duration::from_secs(2))
            .await
            .unwrap();
        assert!(wired.manager.project_state(&project.key).unwrap().held);

        let error = wired
            .manager
            .authorize_intent(root_intent(&project.key, "thread:a"), once_deadline())
            .await
            .unwrap_err();
        assert!(
            error.contains("not lifecycle-ready"),
            "held is not ready: {error}"
        );
        std::mem::forget(dir);
    }

    /// An intent that speaks for another client run is refused.
    #[tokio::test]
    async fn an_intent_for_another_client_run_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let project = scope(dir.path(), "alpha");
        let wired = Wired::connected();
        wired.manager.note_open(&project);
        wired.manager.note_sidecar_epoch("epoch-1");
        wired
            .manager
            .ensure_project(&project, Duration::from_secs(2))
            .await
            .unwrap();

        let mut intent = root_intent(&project.key, "thread:a");
        intent.client_run_id = "someone-else".to_string();
        let error = wired
            .manager
            .authorize_intent(intent, once_deadline())
            .await
            .unwrap_err();
        assert!(error.contains("different client run"), "got: {error}");
        std::mem::forget(dir);
    }

    /// Cmd+Q while a registration is in flight: the late lease must be released
    /// and must never become a ready view.
    ///
    /// The window is real: `register` is a socket round trip, and the user may hit
    /// Cmd+Q inside it. Leaving the lease behind would resurrect a runtime the quit
    /// is already draining.
    #[tokio::test]
    async fn a_quit_during_registration_releases_the_late_lease() {
        let dir = tempfile::tempdir().unwrap();
        let project = scope(dir.path(), "alpha");
        let wired = Wired::connected();
        wired.control.knobs.lock().unwrap().hold_register = true;

        let manager = Arc::clone(&wired.manager);
        let scope_for_task = project.clone();
        let task = tokio::spawn(async move {
            manager
                .ensure_project(&scope_for_task, Duration::from_secs(10))
                .await
        });

        // Wait until the registration is genuinely in flight, so the quit below
        // really lands inside the window rather than before or after it.
        loop {
            if wired.trace().contains(&"register_business") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }

        let manager = Arc::clone(&wired.manager);
        let drain = tokio::spawn(async move {
            manager
                .shutdown(Deadline::after(Duration::from_secs(5)))
                .await
        });
        // Let `shutdown` set the quit flag before the registration is allowed home.
        tokio::time::sleep(Duration::from_millis(20)).await;

        {
            let mut knobs = wired.control.knobs.lock().unwrap();
            knobs.register_released = true;
        }
        let _ = drain.await;

        let view = task.await.unwrap().unwrap();
        assert_eq!(view.state, AcquireState::Blocked, "{view:?}");
        assert!(!wired.manager.is_lifecycle_ready(&project));
        assert!(
            wired.manager.registered_projects().is_empty(),
            "a lease taken during the quit must not stay registered"
        );
        assert!(
            wired.trace().contains(&"release"),
            "the late lease must be released: {:?}",
            wired.trace()
        );
        std::mem::forget(dir);
    }

    /// A project prepared after the quit began is refused without touching a runtime.
    #[tokio::test]
    async fn no_project_is_prepared_after_the_quit_has_begun() {
        let dir = tempfile::tempdir().unwrap();
        let project = scope(dir.path(), "alpha");
        let wired = Wired::connected();
        wired
            .manager
            .shutdown(Deadline::after(Duration::from_secs(1)))
            .await;

        let view = wired
            .manager
            .ensure_project(&project, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(view.state, AcquireState::Blocked);
        assert_eq!(view.retry, RetryClass::DoNotRetry);
        assert_eq!(
            wired.trace(),
            Vec::<&'static str>::new(),
            "quitting must not start a preparation: {:?}",
            wired.trace()
        );
        std::mem::forget(dir);
    }

    /// The view is the only thing the page sees, and it carries no credential.
    #[tokio::test]
    async fn the_page_view_round_trips_and_carries_no_grant() {
        let dir = tempfile::tempdir().unwrap();
        let project = scope(dir.path(), "alpha");
        let wired = Wired::connected();
        let view = wired
            .manager
            .ensure_project(&project, Duration::from_secs(2))
            .await
            .unwrap();

        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["scopeKey"], project.key);
        assert_eq!(json["attemptId"], view.attempt_id);
        assert_eq!(json["state"], "managedReady");
        assert_eq!(json["retry"], "doNotRetry");
        for forbidden in [
            "grant",
            "capability",
            "secret",
            "rootId",
            "daemonInstanceId",
        ] {
            assert!(
                json.get(forbidden).is_none(),
                "{forbidden} must never reach the page: {json}"
            );
        }
        let back: ProjectAcquireView = serde_json::from_value(json).unwrap();
        assert_eq!(back, view);
        std::mem::forget(dir);
    }

    /// A ready project that loses its lease stops being ready on the next check.
    #[tokio::test]
    async fn readiness_is_rechecked_against_the_live_lease() {
        let dir = tempfile::tempdir().unwrap();
        let project = scope(dir.path(), "alpha");
        let wired = Wired::connected();
        wired
            .manager
            .ensure_project(&project, Duration::from_secs(2))
            .await
            .unwrap();
        assert!(wired.manager.is_lifecycle_ready(&project));

        // The guardian withdraws this client: the lease is no longer live.
        wired.control.knobs.lock().unwrap().served = false;
        assert!(
            !wired.manager.is_lifecycle_ready(&project),
            "a lease the guardian no longer honours must not stay ready"
        );
        std::mem::forget(dir);
    }

    /// Two windows opening one project share one preparation.
    #[tokio::test]
    async fn concurrent_attempts_on_one_scope_prepare_once() {
        let dir = tempfile::tempdir().unwrap();
        let project = scope(dir.path(), "alpha");
        let wired = Wired::connected();

        let first = {
            let manager = Arc::clone(&wired.manager);
            let project = project.clone();
            tokio::spawn(async move {
                manager
                    .ensure_project(&project, Duration::from_secs(2))
                    .await
            })
        };
        let second = {
            let manager = Arc::clone(&wired.manager);
            let project = project.clone();
            tokio::spawn(async move {
                manager
                    .ensure_project(&project, Duration::from_secs(2))
                    .await
            })
        };
        let (a, b) = tokio::join!(first, second);
        assert_eq!(a.unwrap().unwrap().state, AcquireState::ManagedReady);
        assert_eq!(b.unwrap().unwrap().state, AcquireState::ManagedReady);

        let preparations = wired
            .trace()
            .iter()
            .filter(|label| **label == "prepare")
            .count();
        assert_eq!(
            preparations,
            1,
            "one project, one preparation: {:?}",
            wired.trace()
        );
        let registrations = wired
            .trace()
            .iter()
            .filter(|label| **label == "register_business")
            .count();
        assert_eq!(
            registrations,
            1,
            "one App, one lease per project: {:?}",
            wired.trace()
        );
        std::mem::forget(dir);
    }

    /// A deadline with no budget left cannot authorize anything.
    fn once_deadline() -> Deadline {
        paused_deadline(Duration::from_secs(5))
    }

    fn root_intent(scope_key: &str, thread_key: &str) -> RootIntentRequest {
        RootIntentRequest {
            sidecar_epoch: "epoch-1".to_string(),
            scope_key: scope_key.to_string(),
            client_run_id: "app-run".to_string(),
            intent: RootBindIntent {
                operation_id: format!("op-{thread_key}"),
                thread_key: thread_key.to_string(),
                workspace: std::path::PathBuf::from("/tmp/workspace"),
                extra_roots: Vec::new(),
                previous_root_capability: None,
            },
        }
    }

    // -----------------------------------------------------------------------
    // Against the real lease table
    // -----------------------------------------------------------------------

    /// A control socket that speaks the real protocol over the real `LeaseTable`.
    ///
    /// The table is the authority on what "registered", "resumed" and "released"
    /// mean, so a fake hand-rolled to the test's expectation could agree with a
    /// wrong shell. Using it makes "the shell registered twice" a real refusal
    /// ("already registered; call resume") rather than a count in a mock.
    struct LeaseGuardian {
        table: Arc<Mutex<yi_agent_lifecycle::leases::LeaseTable>>,
        stop: Arc<std::sync::atomic::AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl LeaseGuardian {
        /// Binds `scope`'s own control socket, so the manager under test talks to it
        /// through the same resolution the packaged shell uses.
        fn start(scope: &RuntimeScope) -> Self {
            std::fs::create_dir_all(&scope.runtime_dir).unwrap();
            let path = yi_agent_lifecycle::scope::control_path(&scope.runtime_dir);
            let _ = std::fs::remove_file(&path);
            let table = Arc::new(Mutex::new(yi_agent_lifecycle::leases::LeaseTable::new()));
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
            listener.set_nonblocking(true).unwrap();

            let table_for_thread = Arc::clone(&table);
            let stop_for_thread = Arc::clone(&stop);
            let scope_key = scope.key.clone();
            let thread = std::thread::spawn(move || {
                while !stop_for_thread.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
                            serve_one(&mut stream, &table_for_thread, &scope_key);
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        Err(_) => return,
                    }
                }
            });
            Self {
                table,
                stop,
                thread: Some(thread),
            }
        }

        fn live_clients(&self) -> Vec<String> {
            self.table
                .lock()
                .unwrap()
                .snapshot()
                .into_iter()
                .map(|(client_run_id, _mode, _connected)| client_run_id)
                .collect()
        }
    }

    impl Drop for LeaseGuardian {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    /// One request on one connection, answered from the real table.
    fn serve_one(
        stream: &mut std::os::unix::net::UnixStream,
        table: &Arc<Mutex<yi_agent_lifecycle::leases::LeaseTable>>,
        scope_key: &str,
    ) {
        use yi_agent_lifecycle::types::{ControlRequest, ControlResponse};
        let frame = match yi_agent_lifecycle::codec::read_frame(stream) {
            Ok(frame) => frame,
            Err(_) => return,
        };
        let Ok(request) = yi_agent_lifecycle::codec::decode::<ControlRequest>(&frame) else {
            return;
        };
        let response = match request {
            ControlRequest::Identity => {
                ControlResponse::Identity(yi_agent_lifecycle::types::RuntimeIdentity {
                    scope_key: scope_key.to_string(),
                    daemon_instance_id: Some("gen-1".to_string()),
                    guardian_build_id: Some("build-b".to_string()),
                    daemon_build_id: Some("build-b".to_string()),
                    mode: yi_agent_lifecycle::types::RuntimeMode::Managed,
                    business_protocol: Some(1),
                    daemon_display_version: Some("0.3.0".to_string()),
                    guardian_display_version: Some("0.3.0".to_string()),
                    capabilities: vec!["lease.v1".to_string(), "control.v1".to_string()],
                    schema_min: 1,
                    schema_max: 16,
                })
            }
            ControlRequest::Register {
                client_run_id,
                kind,
                mode,
            } => match table
                .lock()
                .unwrap()
                .register(client_run_id, kind, mode, Duration::ZERO)
            {
                Ok(grant) => ControlResponse::Granted(grant),
                Err(reason) => ControlResponse::Rejected {
                    code: yi_agent_lifecycle::types::ControlErrorCode::Conflict,
                    reason,
                },
            },
            ControlRequest::Resume { grant } => {
                match table.lock().unwrap().resume(&grant, Duration::ZERO) {
                    Ok(renewed) => ControlResponse::Granted(renewed),
                    Err(reason) => ControlResponse::Rejected {
                        code: yi_agent_lifecycle::types::ControlErrorCode::Unauthenticated,
                        reason,
                    },
                }
            }
            ControlRequest::Heartbeat { grant } => {
                match table.lock().unwrap().heartbeat(&grant, Duration::ZERO) {
                    Ok(()) => ControlResponse::Ack,
                    Err(reason) => ControlResponse::Rejected {
                        code: yi_agent_lifecycle::types::ControlErrorCode::StaleEpoch,
                        reason,
                    },
                }
            }
            ControlRequest::Release { grant } => match table.lock().unwrap().release(&grant) {
                Ok(_) => ControlResponse::ReleaseStarted {
                    operation_id: "op-release".to_string(),
                },
                Err(reason) => ControlResponse::Rejected {
                    code: yi_agent_lifecycle::types::ControlErrorCode::StaleEpoch,
                    reason,
                },
            },
            ControlRequest::Status { .. } => {
                // Readiness re-reads this after the lease is taken, so a fake that
                // stays silent here would fail the identity gate for the wrong
                // reason: silence is not a mismatch.
                ControlResponse::Status(Box::new(status_snapshot_for(scope_key, Some("gen-1"))))
            }
            ControlRequest::ReclaimStatus { operation_id, .. } => ControlResponse::ReclaimStatus {
                operation_id,
                result: Some(yi_agent_lifecycle::types::ReclaimResult {
                    client_run_id: "app-run".to_string(),
                    stopped: true,
                    failures: Vec::new(),
                }),
            },
            ControlRequest::AuthorizeRoot { intent, .. } => {
                ControlResponse::RootAuthorized(yi_agent_lifecycle::types::RootAuthorization {
                    scope_key: scope_key.to_string(),
                    daemon_instance_id: "gen-1".to_string(),
                    client_run_id: "app-run".to_string(),
                    root_id: format!("root-{}", intent.thread_key),
                    capability: "root-capability".to_string(),
                })
            }
            other => ControlResponse::Unsupported {
                detail: format!("this fake does not answer {other:?}"),
            },
        };
        let _ = yi_agent_lifecycle::codec::write_frame(stream, &response);
    }

    /// A manager bound to a real control socket and a fixed preparation answer.
    fn real_socket_manager(
        _guardian: &LeaseGuardian,
        outcome: PrepareOutcome,
    ) -> Arc<DesktopRuntimeManager> {
        let trace = Arc::new(Mutex::new(Vec::new()));
        let prepare = Arc::new(FakePrepare {
            reply: Reply::Fixed(outcome),
            trace,
        });
        Arc::new(
            DesktopRuntimeManager::with_backend("app-run", Arc::new(SocketBackend::new()))
                .with_prepare(prepare)
                .with_artifact(Some(artifact())),
        )
    }

    /// A short-lived directory under `/tmp`.
    ///
    /// Not `tempfile::tempdir()`: its default root is under `/var/folders/...`,
    /// and once the runtime directory and the socket name are appended the path
    /// exceeds `SUN_LEN`, so binding the control socket fails with `InvalidInput`
    /// rather than telling us anything about leases.
    fn short_dir() -> tempfile::TempDir {
        std::fs::create_dir_all("/tmp").unwrap();
        tempfile::Builder::new()
            .prefix("yil")
            .tempdir_in("/tmp")
            .unwrap()
    }

    /// A scope with a real directory under `root`, ready to be served.
    fn real_scope(root: &std::path::Path, name: &str) -> RuntimeScope {
        RuntimeScope {
            key: format!("{}/{name}", root.display()),
            runtime_dir: root.join(name).join(".yi-agent/runtime"),
            project_key: root.join(name).display().to_string(),
            instance_home: root.join("home"),
        }
    }

    /// The happy path over a real socket: one registration, a renewing heartbeat,
    /// and a lease the quit can actually release.
    #[tokio::test]
    async fn a_real_registration_and_heartbeat_reach_ready() {
        let dir = short_dir();
        let project = real_scope(dir.path(), "alpha");
        let guardian = LeaseGuardian::start(&project);
        let manager = real_socket_manager(
            &guardian,
            PrepareOutcome::Connected(managed_identity(&project.key, "gen-1")),
        );

        let view = manager
            .ensure_project(&project, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(view.state, AcquireState::ManagedReady, "{view:?}");
        assert_eq!(guardian.live_clients(), vec!["app-run".to_string()]);
        assert!(manager.is_lifecycle_ready(&project));

        // A repeat call must not register again: the real table refuses a second
        // registration of a live id, so this would surface as a blocked view.
        let again = manager
            .ensure_project(&project, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(again.state, AcquireState::ManagedReady, "{again:?}");
        assert_eq!(
            guardian.live_clients().len(),
            1,
            "one App, one lease per project"
        );
    }

    /// The same App run is never a second client.
    ///
    /// `client_run_id` is minted once per launch. Re-entering `ensure_project` on
    /// the same shell therefore finds its own live lease and never registers again,
    /// and a *second* shell claiming the same id is refused by the real table
    /// ("already registered; call resume") rather than silently owning a duplicate
    /// lease. The refusal is the invariant: re-registering a live id would let a
    /// stale process hold lifetime on a runtime it no longer fronts.
    #[tokio::test]
    async fn a_second_shell_for_the_same_run_is_refused_not_registered_twice() {
        let dir = short_dir();
        let project = real_scope(dir.path(), "alpha");
        let guardian = LeaseGuardian::start(&project);
        let manager = real_socket_manager(
            &guardian,
            PrepareOutcome::Connected(managed_identity(&project.key, "gen-1")),
        );

        let ready = manager
            .ensure_project(&project, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(ready.state, AcquireState::ManagedReady, "{ready:?}");

        // The same shell re-entering the path keeps its own lease and does not go
        // to the guardian at all, so the table still holds exactly one client.
        let again = manager
            .ensure_project(&project, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(again.state, AcquireState::ManagedReady, "{again:?}");
        assert_eq!(guardian.live_clients(), vec!["app-run".to_string()]);

        // A second shell that presents the same id meets a real refusal. It must
        // not become ready on a lease it does not hold.
        let duplicate = real_socket_manager(
            &guardian,
            PrepareOutcome::Connected(managed_identity(&project.key, "gen-1")),
        );
        let view = duplicate
            .ensure_project(&project, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(view.state, AcquireState::Blocked, "{view:?}");
        assert!(
            view.reason
                .unwrap_or_default()
                .contains("already registered"),
            "the refusal must name the reason"
        );
    }

    /// A quit forgets the demand it was told about.
    ///
    /// The release below stops the services; a local table still naming consumers
    /// that no longer exist would describe demand nothing can ever withdraw.
    #[tokio::test]
    async fn a_quit_forgets_local_plugin_demand() {
        let dir = short_dir();
        let project = real_scope(dir.path(), "alpha");
        let guardian = LeaseGuardian::start(&project);
        let manager = real_socket_manager(
            &guardian,
            PrepareOutcome::Connected(managed_identity(&project.key, "gen-1")),
        );
        manager
            .ensure_project(&project, Duration::from_secs(2))
            .await
            .unwrap();
        assert!(
            manager.note_plugin_demand(&project.key, "board", "view-1"),
            "the first consumer must be recorded as the first"
        );

        manager
            .shutdown(Deadline::after(Duration::from_secs(2)))
            .await;

        // A new consumer after the quit starts a fresh count: the old one is gone,
        // so this is again "the first".
        assert!(manager.note_plugin_demand(&project.key, "board", "view-2"));
    }

    /// A quit releases the lease it really holds, over the real socket.
    #[tokio::test]
    async fn a_quit_releases_a_real_lease() {
        let dir = short_dir();
        let project = real_scope(dir.path(), "alpha");
        let guardian = LeaseGuardian::start(&project);
        let manager = real_socket_manager(
            &guardian,
            PrepareOutcome::Connected(managed_identity(&project.key, "gen-1")),
        );
        manager
            .ensure_project(&project, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(guardian.live_clients().len(), 1);

        let results = manager
            .shutdown(Deadline::after(Duration::from_secs(2)))
            .await;
        assert_eq!(results.len(), 1);
        assert_eq!(
            guardian.live_clients(),
            Vec::<String>::new(),
            "the lease must be gone from the real table after the quit"
        );
    }

    /// A held lease is released on quit even when the project is not usable.
    ///
    /// The failure this pins: quitting only the projects that reached
    /// `managedReady` would leave a real registration behind on every blocked
    /// project. The user quit, so the runtime must not stay alive on their behalf --
    /// the reason the project was blocked says nothing about whether we still want it.
    #[tokio::test]
    async fn a_quit_releases_a_held_lease_that_never_became_ready() {
        let dir = short_dir();
        let project = real_scope(dir.path(), "alpha");
        let guardian = LeaseGuardian::start(&project);
        let manager = real_socket_manager(
            &guardian,
            PrepareOutcome::VersionChoiceRequired(managed_identity(&project.key, "gen-1")),
        );

        let view = manager
            .ensure_project(&project, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(view.state, AcquireState::Blocked, "{view:?}");
        // The lease is real even though the project is not ready: that is the point.
        assert_eq!(guardian.live_clients(), vec!["app-run".to_string()]);

        let results = manager
            .shutdown(Deadline::after(Duration::from_secs(2)))
            .await;
        assert_eq!(results.len(), 1);
        assert!(
            results[0].stopped,
            "a quit that leaves a blocked project registered did not stop it"
        );
        assert_eq!(
            guardian.live_clients(),
            Vec::<String>::new(),
            "a blocked project still owed its lease back on quit"
        );
    }

    /// A compatible mismatch holds a real lease, and that lease is what the
    /// existing preview/confirm flow presents.
    #[tokio::test]
    async fn a_compatible_mismatch_holds_a_real_lease_while_blocked() {
        let dir = short_dir();
        let project = real_scope(dir.path(), "alpha");
        let guardian = LeaseGuardian::start(&project);
        let manager = real_socket_manager(
            &guardian,
            PrepareOutcome::VersionChoiceRequired(managed_identity(&project.key, "gen-1")),
        );

        let view = manager
            .ensure_project(&project, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(view.state, AcquireState::Blocked);
        assert_eq!(
            guardian.live_clients(),
            vec!["app-run".to_string()],
            "the mismatch may use its own lease to switch"
        );
        // The lease is real, so a preview could be requested with it.
        assert!(manager.lease_for_recovery(&project).is_ok());
        // But nothing that runs work may pass.
        assert!(!manager.is_lifecycle_ready(&project));
        manager.note_open(&project);
        manager.note_sidecar_epoch("epoch-1");
        let error = manager
            .authorize_intent(root_intent(&project.key, "thread:a"), once_deadline())
            .await
            .unwrap_err();
        assert!(error.contains("not lifecycle-ready"), "got: {error}");
    }

    /// A ready project authorizes a root through the real socket.
    #[tokio::test]
    async fn a_ready_project_can_authorize_a_root() {
        let dir = short_dir();
        let project = real_scope(dir.path(), "alpha");
        let guardian = LeaseGuardian::start(&project);
        let manager = real_socket_manager(
            &guardian,
            PrepareOutcome::Connected(managed_identity(&project.key, "gen-1")),
        );
        manager.note_open(&project);
        manager.note_sidecar_epoch("epoch-1");
        manager
            .ensure_project(&project, Duration::from_secs(2))
            .await
            .unwrap();

        let authorization = manager
            .authorize_intent(root_intent(&project.key, "thread:a"), once_deadline())
            .await
            .unwrap();
        assert_eq!(authorization.key.root_id, "root-thread:a");
        assert_eq!(authorization.key.client_run_id, "app-run");
        assert_eq!(authorization.key.scope_key, project.key);
    }

    /// Cancelling the version choice leaves the project blocked, and a quit still
    /// releases the lease that was held for the preview.
    #[tokio::test]
    async fn a_cancelled_version_choice_keeps_the_lease_for_the_quit_to_release() {
        let dir = short_dir();
        let project = real_scope(dir.path(), "alpha");
        let guardian = LeaseGuardian::start(&project);
        let manager = real_socket_manager(
            &guardian,
            PrepareOutcome::VersionChoiceRequired(managed_identity(&project.key, "gen-1")),
        );
        manager
            .ensure_project(&project, Duration::from_secs(2))
            .await
            .unwrap();

        // The user does nothing: no switch, no adoption. The state must stay
        // blocked, but the lease is still real and still the quit's to release.
        let state = manager.project_state(&project.key).unwrap();
        assert_eq!(state.view.state, AcquireState::Blocked);
        assert!(state.held);
        let results = manager
            .shutdown(Deadline::after(Duration::from_secs(2)))
            .await;
        assert_eq!(results.len(), 1);
        assert_eq!(guardian.live_clients(), Vec::<String>::new());
    }

    /// A waiting-next-generation grant is never a lease this shell is ready on.
    ///
    /// The mode means "you are queued behind a switch", which is the opposite of
    /// ready: the runtime is actively being replaced.
    #[tokio::test]
    async fn a_waiting_next_generation_grant_is_never_ready() {
        let dir = short_dir();
        let project = real_scope(dir.path(), "alpha");
        let guardian = LeaseGuardian::start(&project);
        // Pre-register the App in the real table with the waiting mode, so the
        // shell's later attempt meets the queued grant the table really hands out.
        guardian
            .table
            .lock()
            .unwrap()
            .register(
                "app-run".to_string(),
                ClientKind::App,
                LeaseMode::WaitingNextGeneration,
                Duration::ZERO,
            )
            .unwrap();
        let manager = real_socket_manager(
            &guardian,
            PrepareOutcome::Connected(managed_identity(&project.key, "gen-1")),
        );

        let view = manager
            .ensure_project(&project, Duration::from_secs(2))
            .await
            .unwrap();
        assert_ne!(
            view.state,
            AcquireState::ManagedReady,
            "a queued client is not a ready one: {view:?}"
        );
        assert!(!manager.is_lifecycle_ready(&project));
    }
}

// ---------------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------------

/// Where a project's runtime directory lives: `YI_AGENT_RUNTIME_DIR` if set,
/// otherwise `<project>/.yi-agent/runtime`.
///
/// The shell asks rather than guesses: this is the one place the desktop knows
/// about runtime layout, and it defers to the shared resolver.
fn runtime_dir_for(project: &std::path::Path) -> std::path::PathBuf {
    yi_agent_lifecycle::scope::runtime_directory(project)
}

/// The instance home this shell is running as.
///
/// Mirrors `bridge::instance_home`: `YI_AGENT_HOME` wins, then the user's home.
/// Two instances must never silently share one runtime, and the scope records
/// this value so a mismatch is a visible conflict rather than a shared database.
fn instance_home() -> std::io::Result<std::path::PathBuf> {
    if let Some(value) = std::env::var_os("YI_AGENT_HOME").filter(|value| !value.is_empty()) {
        return Ok(std::path::PathBuf::from(value));
    }
    dirs::home_dir().ok_or_else(|| std::io::Error::other("the home directory is unavailable"))
}

// ---------------------------------------------------------------------------
// Wire views
//
// The desktop commands must answer in the *same* shape as
// `runtime/status` (see `yi_agent_app_server::runtime_status`), because one React
// component renders both: the banner shows the app-server's view when it is talking
// to a sidecar and this shell's view when the sidecar is down. Mirrors rather than
// re-exports: the app-server crate carries the whole agent runtime, and the shell
// deliberately does not link it. The field names are pinned by a test against the
// generated TypeScript, so a mirror cannot drift silently.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeIdentityView {
    pub scope_key: String,
    pub daemon_instance_id: Option<String>,
    pub guardian_build_id: Option<String>,
    pub daemon_build_id: Option<String>,
    /// `managed` / `legacy_independent` / `legacy_embedded` / `unknown`.
    pub mode: String,
    pub business_protocol: Option<u32>,
    pub daemon_display_version: Option<String>,
    pub guardian_display_version: Option<String>,
    pub capabilities: Vec<String>,
    pub schema_min: u32,
    pub schema_max: u32,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImpactCountsView {
    pub clients: u64,
    pub pending_clients: u64,
    pub running: u64,
    pub queued: u64,
    pub plugins: u64,
    pub complete: bool,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientSummaryView {
    pub client_run_id: String,
    pub kind: String,
    pub mode: String,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImpactRevisionView {
    pub guardian: u64,
    pub coordinator: Option<u64>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImpactEntryView {
    pub kind: String,
    pub id: String,
    pub owner: Option<String>,
}

/// Mirrors `RestartPreview` (`RuntimeRestartPreviewView` in the app-server).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestartPreviewView {
    pub snapshot_id: String,
    pub token: String,
    pub revision: ImpactRevisionView,
    pub counts: ImpactCountsView,
    pub entries: Vec<ImpactEntryView>,
    pub next_cursor: Option<String>,
    pub requires_second_confirmation: bool,
    pub can_restart: bool,
    pub blocked_reason: Option<String>,
}

/// Mirrors `OperationStatus` (`RuntimeOperationStatusView` in the app-server).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationStatusView {
    pub operation_id: String,
    pub phase: String,
    pub detail: Option<String>,
    pub ready_identity: Option<RuntimeIdentityView>,
}

/// Mirrors `RuntimeStatusView`.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeStatusView {
    pub identity: RuntimeIdentityView,
    pub expected_build_id: Option<String>,
    pub expected_display_version: Option<String>,
    /// The wire names of `Compatibility` as ts-rs generates them
    /// (`sameBuild` / `compatibleMismatch` / `controlOnly` /
    /// `legacyTemporaryChoiceRequired` / `unknown`). They are camelCase, unlike the
    /// mode and kind enums: one component renders both this shell's view and the
    /// app-server's, so the two must spell the same value the same way.
    pub compatibility: String,
    pub lifecycle_managed: bool,
    pub counts: ImpactCountsView,
    pub clients: Vec<ClientSummaryView>,
    pub clients_next_cursor: Option<String>,
    pub active_operation: Option<OperationStatusView>,
    pub local_restart_available: bool,
    pub message: Option<String>,
}

fn identity_view(identity: &yi_agent_lifecycle::types::RuntimeIdentity) -> RuntimeIdentityView {
    use yi_agent_lifecycle::types::RuntimeMode;
    RuntimeIdentityView {
        scope_key: identity.scope_key.clone(),
        daemon_instance_id: identity.daemon_instance_id.clone(),
        guardian_build_id: identity.guardian_build_id.clone(),
        daemon_build_id: identity.daemon_build_id.clone(),
        mode: match identity.mode {
            RuntimeMode::Managed => "managed",
            RuntimeMode::LegacyIndependent => "legacy_independent",
            RuntimeMode::LegacyEmbedded => "legacy_embedded",
            RuntimeMode::Unknown => "unknown",
        }
        .to_string(),
        business_protocol: identity.business_protocol,
        daemon_display_version: identity.daemon_display_version.clone(),
        guardian_display_version: identity.guardian_display_version.clone(),
        capabilities: identity.capabilities.clone(),
        schema_min: identity.schema_min,
        schema_max: identity.schema_max,
    }
}

fn counts_view(counts: &yi_agent_lifecycle::types::ImpactCounts) -> ImpactCountsView {
    ImpactCountsView {
        clients: counts.clients,
        pending_clients: counts.pending_clients,
        running: counts.running,
        queued: counts.queued,
        plugins: counts.plugins,
        complete: counts.complete,
    }
}

fn operation_view(status: &yi_agent_lifecycle::types::OperationStatus) -> OperationStatusView {
    OperationStatusView {
        operation_id: status.operation_id.clone(),
        // The snake-case wire name, like the app-server's view: the phase list
        // grows with the state machine, and a second enum here would mislabel a
        // phase the moment one is added.
        phase: serde_json::to_value(status.phase)
            .ok()
            .and_then(|value| value.as_str().map(str::to_string))
            .unwrap_or_else(|| "unknown".to_string()),
        detail: status.detail.clone(),
        ready_identity: status.ready_identity.as_ref().map(identity_view),
    }
}

fn runtime_operation_view(status: yi_agent_lifecycle::types::OperationStatus) -> serde_json::Value {
    serde_json::to_value(operation_view(&status)).unwrap_or(serde_json::Value::Null)
}

fn runtime_preview_view(preview: yi_agent_lifecycle::types::RestartPreview) -> serde_json::Value {
    let view = RestartPreviewView {
        snapshot_id: preview.snapshot_id,
        token: preview.token,
        revision: ImpactRevisionView {
            guardian: preview.revision.guardian,
            coordinator: preview.revision.coordinator,
        },
        counts: counts_view(&preview.counts),
        entries: preview
            .entries
            .into_iter()
            .map(|entry| ImpactEntryView {
                kind: entry.kind,
                id: entry.id,
                owner: entry.owner,
            })
            .collect(),
        next_cursor: preview.next_cursor,
        requires_second_confirmation: preview.requires_second_confirmation,
        can_restart: preview.can_restart,
        blocked_reason: preview.blocked_reason,
    };
    serde_json::to_value(view).unwrap_or(serde_json::Value::Null)
}

/// The status view this shell renders when the sidecar is down.
///
/// `unreachable` is the explanation, and it is *not* a failure of the command: the
/// banner has to say "I cannot tell" rather than "everything is fine", so the answer
/// is a view with `compatibility: "unknown"`.
fn runtime_view(
    manager: &DesktopRuntimeManager,
    scope: &RuntimeScope,
    snapshot: Option<RuntimeStatusSnapshot>,
    unreachable: Option<String>,
) -> Result<serde_json::Value, String> {
    let artifact = manager.artifact();
    let view = match snapshot {
        None => RuntimeStatusView {
            identity: RuntimeIdentityView {
                scope_key: scope.key.clone(),
                daemon_instance_id: None,
                guardian_build_id: None,
                daemon_build_id: None,
                mode: "unknown".to_string(),
                business_protocol: None,
                daemon_display_version: None,
                guardian_display_version: None,
                capabilities: Vec::new(),
                schema_min: 0,
                schema_max: 0,
            },
            expected_build_id: artifact.map(|artifact| artifact.build_id.clone()),
            expected_display_version: artifact.map(|artifact| artifact.display_version.clone()),
            compatibility: "unknown".to_string(),
            lifecycle_managed: false,
            counts: ImpactCountsView {
                clients: 0,
                pending_clients: 0,
                running: 0,
                queued: 0,
                plugins: 0,
                complete: false,
            },
            clients: Vec::new(),
            clients_next_cursor: None,
            active_operation: None,
            local_restart_available: false,
            message: Some(
                unreachable.unwrap_or_else(|| "no runtime answered for this project".to_string()),
            ),
        },
        Some(snapshot) => {
            // Same rule as the app-server: no verified expectation is `unknown`, not
            // "compatible". A dev build must not claim a match it never checked.
            let compatibility = artifact
                .map(|artifact| classify_against(&snapshot.identity, artifact))
                .unwrap_or_else(|| "unknown".to_string());
            let lifecycle_managed =
                snapshot.identity.mode == yi_agent_lifecycle::types::RuntimeMode::Managed;
            RuntimeStatusView {
                identity: identity_view(&snapshot.identity),
                expected_build_id: artifact.map(|artifact| artifact.build_id.clone()),
                expected_display_version: artifact.map(|artifact| artifact.display_version.clone()),
                message: status_message(&compatibility, lifecycle_managed),
                compatibility,
                lifecycle_managed,
                counts: counts_view(&snapshot.counts),
                clients: snapshot
                    .clients
                    .iter()
                    .map(|client| ClientSummaryView {
                        client_run_id: client.client_run_id.clone(),
                        kind: match client.kind {
                            ClientKind::App => "app",
                            ClientKind::Tui => "tui",
                            ClientKind::Headless => "headless",
                        }
                        .to_string(),
                        mode: match client.mode {
                            LeaseMode::Business => "business",
                            LeaseMode::LeaseOnly => "lease_only",
                            LeaseMode::WaitingNextGeneration => "waiting_next_generation",
                        }
                        .to_string(),
                    })
                    .collect(),
                clients_next_cursor: snapshot.clients_next_cursor.clone(),
                active_operation: snapshot.active_operation.as_ref().map(operation_view),
                // This is the local shell: the control socket is ours, so the button
                // is real.
                local_restart_available: snapshot.local_restart_available,
            }
        }
    };
    serde_json::to_value(view).map_err(|error| error.to_string())
}

/// Classifies a running identity against this shell's artifact.
///
/// The shell cannot link the supervisors (they carry the store), so the rule is
/// mirrored here. It is the same three questions in the same order: can this build's
/// schema be read, is this a managed runtime at all, and do the bytes and protocol
/// match what we shipped.
fn classify_against(
    identity: &yi_agent_lifecycle::types::RuntimeIdentity,
    expected: &ArtifactIdentity,
) -> String {
    use yi_agent_lifecycle::types::RuntimeMode;
    if identity.mode == RuntimeMode::Unknown || identity.daemon_build_id.is_none() {
        return "unknown".to_string();
    }
    if identity.schema_max < expected.schema_min || identity.schema_min > expected.schema_max {
        return "unknown".to_string();
    }
    if matches!(
        identity.mode,
        RuntimeMode::LegacyIndependent | RuntimeMode::LegacyEmbedded
    ) {
        return "legacyTemporaryChoiceRequired".to_string();
    }
    let business_usable = identity.business_protocol.is_some_and(|protocol| {
        protocol >= expected.business_protocol_min && protocol <= expected.business_protocol_max
    });
    if business_usable {
        let same_daemon = identity.daemon_build_id.as_deref() == Some(expected.build_id.as_str());
        let same_guardian =
            identity.guardian_build_id.as_deref() == Some(expected.build_id.as_str());
        if same_daemon && same_guardian {
            return "sameBuild".to_string();
        }
        return "compatibleMismatch".to_string();
    }
    if identity
        .capabilities
        .iter()
        .any(|capability| capability == "control.v1")
    {
        return "controlOnly".to_string();
    }
    "unknown".to_string()
}

/// The banner's sentence, matching the app-server's wording.
fn status_message(compatibility: &str, lifecycle_managed: bool) -> Option<String> {
    match compatibility {
        "sameBuild" => None,
        "compatibleMismatch" => Some(
            "this project's runtime is a different build; new features may be unavailable"
                .to_string(),
        ),
        "controlOnly" => Some(
            "the background runtime's business protocol is incompatible: tasks are disabled \
             until it is restarted"
                .to_string(),
        ),
        "legacyTemporaryChoiceRequired" => Some(
            "this runtime was started by an older version and has no exit guarantees; \
             stop it by hand to get managed restarts"
                .to_string(),
        ),
        _ if !lifecycle_managed => Some("the running runtime could not be identified".to_string()),
        _ => Some("the running runtime's build could not be verified".to_string()),
    }
}

/// Maps a WebView-supplied scope key back to a scope this shell resolved itself.
///
/// The key is only a lookup, never a path: a WebView that invents a key gets
/// "not registered" rather than a control request aimed at a directory of its
/// choosing. This is the reason the commands below take `scope_key` at all instead
/// of accepting a project path or an executable.
fn registered_scope(state: &QuitState, scope_key: &str) -> Result<RuntimeScope, String> {
    state
        .manager
        .known_scopes()
        .into_iter()
        .find(|scope| scope.key == scope_key)
        .ok_or_else(|| {
            format!(
                "project `{scope_key}` is not open in this app, so it has no runtime to talk to"
            )
        })
}

/// Read-only runtime status for one open project.
///
/// Read-only at every layer: no lease is taken, none is renewed, and the answer may
/// honestly be "nothing here" (embedded or legacy runtime).
#[tauri::command]
pub async fn runtime_status(
    state: tauri::State<'_, QuitState>,
    scope_key: String,
) -> Result<serde_json::Value, String> {
    let scope = registered_scope(&state, &scope_key)?;
    let manager = Arc::clone(&state.manager);
    let snapshot = manager
        .status(&scope, STATUS_BUDGET)
        .await
        .map_err(|error| error.to_string())?;
    runtime_view(&manager, &scope, snapshot, None)
}

/// Read-only preview of replacing this project's runtime with the shell's own.
///
/// The target comes from the shell's verified manifest, never from the WebView: a
/// caller-supplied executable would be "run this binary" behind a friendlier name.
#[tauri::command]
pub async fn runtime_preview(
    state: tauri::State<'_, QuitState>,
    scope_key: String,
) -> Result<serde_json::Value, String> {
    let scope = registered_scope(&state, &scope_key)?;
    let manager = Arc::clone(&state.manager);
    let preview = manager
        .preview(&scope, STATUS_BUDGET)
        .await
        .map_err(|error| error.to_string())?;
    Ok(runtime_preview_view(preview))
}

/// Consumes a preview token and starts the switch.
///
/// `operation_id` is supplied by the caller so it exists before the click; if this
/// call's answer is lost, the caller polls [`runtime_operation`] with the same id
/// instead of asking for a second confirmation.
#[tauri::command]
pub async fn runtime_confirm(
    state: tauri::State<'_, QuitState>,
    scope_key: String,
    token: String,
    operation_id: String,
) -> Result<serde_json::Value, String> {
    let scope = registered_scope(&state, &scope_key)?;
    if token.trim().is_empty() {
        return Err("a confirmation needs the token from the preview it was shown".to_string());
    }
    if operation_id.trim().is_empty() {
        return Err("a confirmation needs the operation id chosen before the click".to_string());
    }
    let manager = Arc::clone(&state.manager);
    let confirmed = manager
        .confirm(&scope, &token, &operation_id, SWITCH_BUDGET)
        .await
        .map_err(|error| error.to_string())?;
    Ok(serde_json::json!({ "operationId": confirmed }))
}

/// One read of an operation's progress.
///
/// `Ok(null)` means this runtime never ran that id -- which is *not* the same as
/// "it did not happen": the runtime may have restarted since. The caller must say
/// "unknown" rather than "failed".
#[tauri::command]
pub async fn runtime_operation(
    state: tauri::State<'_, QuitState>,
    scope_key: String,
    operation_id: String,
) -> Result<serde_json::Value, String> {
    let scope = registered_scope(&state, &scope_key)?;
    let manager = Arc::clone(&state.manager);
    let status = manager
        .operation(&scope, &operation_id, STATUS_BUDGET)
        .await
        .map_err(|error| error.to_string())?;
    Ok(match status {
        Some(status) => runtime_operation_view(status),
        None => serde_json::Value::Null,
    })
}

/// The operation this shell last started for `scope_key`, if any.
///
/// Exists so a WebView reload can resume polling instead of forgetting that a
/// switch is in flight.
#[tauri::command]
pub fn runtime_active_operation(
    state: tauri::State<'_, QuitState>,
    scope_key: String,
) -> Option<String> {
    state.manager.operation_for(&scope_key)
}

/// Prepares and leases `project` for this App run, reporting the outcome.
///
/// Called when the user actually starts using a project (opening it, or working
/// in it). A project merely present in an index is never acquired: a historical
/// entry must not keep a runtime alive after everyone has left.
///
/// The answer is the *view*, not `()`. Acquiring used to be fire-and-forget, which
/// meant the page treated "the command returned" as "this project is ready" -- so a
/// blocked or unknown outcome silently looked like a live runtime. `managedReady` is
/// the only state that means work may start.
#[tauri::command]
pub async fn runtime_acquire_project(
    state: tauri::State<'_, QuitState>,
    project: String,
) -> Result<ProjectAcquireView, String> {
    let manager = Arc::clone(&state.manager);
    let home = instance_home().map_err(|error| error.to_string())?;
    let project = std::path::PathBuf::from(project);
    let runtime = runtime_dir_for(&project);
    let scope = yi_agent_lifecycle::scope::resolve(&runtime, &project, &home)
        .map_err(|error| error.to_string())?;
    // Readable before it is leased: a runtime that refuses registration (embedded,
    // legacy) still has a version the banner must be able to report.
    manager.note_open(&scope);
    // The management budget, not the control timeout: preparation has to cold-start
    // a runtime, and the 2s control round-trip would time out on every first launch.
    manager
        .ensure_project(&scope, crate::runtime_management::MANAGEMENT_BUDGET)
        .await
}

/// Releases one project's lease, without quitting.
///
/// Used when the user leaves a project **and** it has no background work left.
/// Runs under its own bounded budget, so a wedged daemon delays only this call.
#[tauri::command]
pub async fn runtime_release_project(
    state: tauri::State<'_, QuitState>,
    scope_key: String,
) -> Result<Vec<String>, String> {
    let manager = Arc::clone(&state.manager);
    let result = manager
        .release_project(&scope_key, Deadline::after(ACQUIRE_BUDGET))
        .await
        .map_err(|error| error.to_string())?;
    // No longer open, so no longer describable: a closed project must not keep
    // answering status reads with a runtime nobody is looking at.
    manager.forget_open(&scope_key);
    Ok(result.failures)
}

/// The projects this App run currently holds. Empty before the first acquire.
#[tauri::command]
pub fn runtime_registered_projects(state: tauri::State<'_, QuitState>) -> Vec<String> {
    state
        .manager
        .registered_projects()
        .into_iter()
        .map(|scope| scope.key)
        .collect()
}

/// Declares that one UI consumer now wants `plugin`'s service for a project.
///
/// The page names a *consumer*, not a lease: demand has no credential and cannot
/// keep a runtime alive on its own. The shell forwards only when this consumer is
/// the first for that `(scope, plugin)` -- a second view joining changes nothing
/// about the service, and a per-view acquire would make the native side's "last
/// holder releases" rule mean something else.
#[tauri::command]
pub async fn runtime_plugin_demand_acquire(
    state: tauri::State<'_, QuitState>,
    scope_key: String,
    plugin: String,
    consumer: String,
) -> Result<serde_json::Value, String> {
    let scope = registered_scope(&state, &scope_key)?;
    let manager = Arc::clone(&state.manager);
    if plugin.trim().is_empty() || consumer.trim().is_empty() {
        return Err("plugin demand needs both a plugin and a consumer".to_string());
    }
    let first = manager.note_plugin_demand(&scope.key, &plugin, &consumer);
    let (wanted, changed) = if first {
        let backend = Arc::clone(&manager.backend);
        let scope_for_call = scope.clone();
        let plugin_for_call = plugin.clone();
        match tokio::task::spawn_blocking(move || {
            backend.note_plugin_demand(&scope_for_call, &plugin_for_call, true, ACQUIRE_BUDGET)
        })
        .await
        {
            Ok(Ok(answer)) => answer,
            // The local intent is kept even when the guardian cannot be told: the
            // runtime may be coming up, and a dropped intent would leave the view
            // waiting for a service nobody asked for.
            Ok(Err(error)) => {
                return Ok(serde_json::json!({
                    "plugin": plugin,
                    "wanted": true,
                    "changed": true,
                    "deferred": error.to_string(),
                }))
            }
            Err(join) => {
                return Ok(serde_json::json!({
                    "plugin": plugin,
                    "wanted": true,
                    "changed": true,
                    "deferred": format!("the demand task failed: {join}"),
                }))
            }
        }
    } else {
        (true, false)
    };
    Ok(serde_json::json!({
        "plugin": plugin,
        "wanted": wanted,
        "changed": changed,
    }))
}

/// Withdraws one consumer's demand.
///
/// Only the last consumer of a plugin is forwarded as a release. A view closing
/// while another view still uses the same service must not stop it.
#[tauri::command]
pub async fn runtime_plugin_demand_release(
    state: tauri::State<'_, QuitState>,
    scope_key: String,
    plugin: String,
    consumer: String,
) -> Result<serde_json::Value, String> {
    let scope = registered_scope(&state, &scope_key)?;
    let manager = Arc::clone(&state.manager);
    let last = manager.drop_plugin_demand(&scope.key, &plugin, &consumer);
    if !last {
        // Either another consumer still wants it, or this one never registered.
        // Both mean "nothing to tell the guardian", and both must NOT be reported
        // as a stop.
        return Ok(serde_json::json!({
            "plugin": plugin,
            "wanted": true,
            "changed": false,
        }));
    }
    let backend = Arc::clone(&manager.backend);
    let scope_for_call = scope.clone();
    let plugin_for_call = plugin.clone();
    let (wanted, changed) = match tokio::task::spawn_blocking(move || {
        backend.note_plugin_demand(&scope_for_call, &plugin_for_call, false, ACQUIRE_BUDGET)
    })
    .await
    {
        Ok(Ok(answer)) => answer,
        Ok(Err(error)) => {
            return Ok(serde_json::json!({
                "plugin": plugin,
                "wanted": false,
                "changed": true,
                "deferred": error.to_string(),
            }))
        }
        Err(join) => {
            return Ok(serde_json::json!({
                "plugin": plugin,
                "wanted": false,
                "changed": true,
                "deferred": format!("the demand task failed: {join}"),
            }))
        }
    };
    Ok(serde_json::json!({
        "plugin": plugin,
        "wanted": wanted,
        "changed": changed,
    }))
}

/// Read-only preview of migrating this project's legacy runtime to the shell's own.
///
/// The target comes from the shell's verified manifest, never from the WebView: a
/// caller-supplied executable would be "replace the runtime with this binary".
#[tauri::command]
pub async fn runtime_migration_preview(
    state: tauri::State<'_, QuitState>,
    scope_key: String,
) -> Result<serde_json::Value, String> {
    let scope = registered_scope(&state, &scope_key)?;
    preview_migration(&state.manager, scope).await
}

/// Consumes a preview token and runs (or reconciles) one migration.
///
/// `operation_id` is chosen by the caller before the request goes out, and recorded
/// here before the frame is written. That ordering is the whole safety property: if
/// this call's answer is lost, the caller polls [`runtime_migration_operation`] with
/// the same id. Sending a second confirmation would be a second migration.
#[tauri::command]
pub async fn runtime_migration_confirm(
    state: tauri::State<'_, QuitState>,
    scope_key: String,
    token: String,
    operation_id: String,
    temporary_use: Option<bool>,
) -> Result<serde_json::Value, String> {
    let scope = registered_scope(&state, &scope_key)?;
    confirm_migration(
        &state.manager,
        scope,
        &token,
        &operation_id,
        temporary_use.unwrap_or(false),
    )
    .await
}

/// One read of a migration operation. Changes nothing.
#[tauri::command]
pub async fn runtime_migration_operation(
    state: tauri::State<'_, QuitState>,
    scope_key: String,
    operation_id: String,
) -> Result<serde_json::Value, String> {
    let _ = registered_scope(&state, &scope_key)?;
    read_migration_operation(&state.manager, &scope_key, &operation_id).await
}

/// The migration target: this App's own verified artifact.
///
/// Refusing without one is deliberate. "Migrate to whatever is running" is exactly
/// the guess that a version string would let us make silently.
fn migration_target(manager: &DesktopRuntimeManager) -> Result<ArtifactIdentity, String> {
    manager.artifact().cloned().ok_or_else(|| {
        "this build has no verified runtime artifact, so it cannot migrate one".to_string()
    })
}

fn migration_channel(manager: &DesktopRuntimeManager) -> Result<Arc<dyn MigrationChannel>, String> {
    manager.migration_channel().ok_or_else(|| {
        "this build has no local management channel, so it cannot inspect a migration".to_string()
    })
}

/// The preview, with no side effects at all.
async fn preview_migration(
    manager: &DesktopRuntimeManager,
    scope: RuntimeScope,
) -> Result<serde_json::Value, String> {
    let target = migration_target(manager)?;
    let channel = migration_channel(manager)?;
    let request = yi_agent_lifecycle::production::MigrationPreviewRequest { scope, target };
    let params = serde_json::to_value(request).map_err(|error| error.to_string())?;
    let answer = tokio::task::spawn_blocking(move || {
        channel.call(
            crate::runtime_management::MIGRATION_PREVIEW_METHOD,
            params,
            MIGRATION_BUDGET,
        )
    })
    .await
    .map_err(|error| format!("the migration preview task failed: {error}"))?
    .map_err(|error| error.to_string())?;
    answer.ok_or_else(|| {
        "the local management channel did not answer the preview, so this runtime cannot say what a migration would touch".to_string()
    })
}

/// One confirmation, or a reconciliation of the id it names.
async fn confirm_migration(
    manager: &DesktopRuntimeManager,
    scope: RuntimeScope,
    token: &str,
    operation_id: &str,
    temporary_use: bool,
) -> Result<serde_json::Value, String> {
    if token.trim().is_empty() {
        return Err("a migration needs the token from the preview it was shown".to_string());
    }
    if operation_id.trim().is_empty() {
        return Err("a migration needs the operation id chosen before the click".to_string());
    }
    let target = migration_target(manager)?;
    let channel = migration_channel(manager)?;
    // Recorded before the request is sent: a WebView reload must be able to find the
    // operation it started, and an answer lost in flight must be reconcilable.
    manager.note_migration_operation(&scope.key, operation_id);
    let request = yi_agent_lifecycle::production::MigrationConfirmRequest {
        scope,
        target,
        token: token.to_string(),
        operation_id: operation_id.to_string(),
        temporary_use,
    };
    let params = serde_json::to_value(request).map_err(|error| error.to_string())?;
    let answer = tokio::task::spawn_blocking(move || {
        channel.call(
            crate::runtime_management::MIGRATION_CONFIRM_METHOD,
            params,
            MIGRATION_BUDGET,
        )
    })
    .await
    .map_err(|error| format!("the migration confirmation task failed: {error}"))?
    .map_err(|error| error.to_string())?;
    Ok(match answer {
        Some(result) => result,
        // No answer is not a failure: the migration may well be running. Report the
        // id so the caller reconciles instead of confirming again.
        None => serde_json::json!({
            "operationId": operation_id,
            "phase": "unknown",
            "detail": "the local management channel did not answer; poll this operation id",
        }),
    })
}

/// One read of an operation, plus the bookkeeping that ends a recorded one.
async fn read_migration_operation(
    manager: &DesktopRuntimeManager,
    scope_key: &str,
    operation_id: &str,
) -> Result<serde_json::Value, String> {
    let channel = migration_channel(manager)?;
    let request = yi_agent_lifecycle::production::MigrationOperationRequest {
        scope_key: scope_key.to_string(),
        operation_id: operation_id.to_string(),
    };
    let params = serde_json::to_value(request).map_err(|error| error.to_string())?;
    let answer = tokio::task::spawn_blocking(move || {
        channel.call(
            crate::runtime_management::MIGRATION_OPERATION_METHOD,
            params,
            STATUS_BUDGET,
        )
    })
    .await
    .map_err(|error| format!("the migration read task failed: {error}"))?
    .map_err(|error| error.to_string())?;
    let Some(result) = answer else {
        return Ok(serde_json::Value::Null);
    };
    // A terminal phase ends the operation, so the shell stops offering to resume it.
    // Everything else stays recorded: an in-flight migration must remain visible,
    // which is what prevents a second confirmation.
    if result
        .get("phase")
        .and_then(|phase| phase.as_str())
        .is_some_and(is_terminal_migration_phase)
    {
        manager.forget_migration(scope_key);
    }
    Ok(result)
}

/// Whether a migration phase means the operation is over.
///
/// Named rather than inlined because "is it done" and "is it still moving"
/// disagreeing is how a UI ends up polling forever -- or, worse, offering to confirm
/// a migration that is still stopping something.
fn is_terminal_migration_phase(phase: &str) -> bool {
    matches!(
        phase,
        "completed" | "failed" | "temporary_use" | "manual_action_required"
    )
}

/// The migration this shell last started for `scope_key`, if any.
///
/// Exists so a WebView reload resumes polling instead of forgetting that a migration
/// is in flight, and so it does not offer a second confirmation for one already sent.
#[tauri::command]
pub fn runtime_active_migration(
    state: tauri::State<'_, QuitState>,
    scope_key: String,
) -> Option<String> {
    state.manager.migration_operation_for(&scope_key)
}

/// The scope keys this shell can answer for.
///
/// The frontend needs a scope key to poll status, but it must not compute one: the
/// key is a canonical runtime directory, and computing it in the WebView (with a
/// different canonicalisation) would name a scope the shell has never resolved.
/// It asks the shell instead, and the shell answers only about projects the user
/// actually opened.
#[tauri::command]
pub fn runtime_known_scopes(state: tauri::State<'_, QuitState>) -> Vec<String> {
    state
        .manager
        .known_scopes()
        .into_iter()
        .map(|scope| scope.key)
        .collect()
}

/// The scope key for one project path, once this shell has resolved it.
#[tauri::command]
pub fn runtime_scope_for_project(
    state: tauri::State<'_, QuitState>,
    project: String,
) -> Option<String> {
    let project = std::path::PathBuf::from(project);
    state
        .manager
        .known_scopes()
        .into_iter()
        .find(|scope| scope.project_key == project.to_string_lossy())
        .map(|scope| scope.key)
}

/// The result of the last quit, for the UI and for support reports.
#[tauri::command]
pub fn runtime_quit_outcome(state: tauri::State<'_, QuitState>) -> serde_json::Value {
    let outcome = state.outcome();
    serde_json::json!({
        "clean": outcome.clean,
        "results": outcome.results,
    })
}
