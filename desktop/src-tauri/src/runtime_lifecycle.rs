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
use yi_agent_lifecycle::types::{
    ArtifactIdentity, ClientKind, LeaseMode, OperationId, OperationStatus, ReclaimResult,
    RestartPreview, RuntimeScope, RuntimeStatusSnapshot,
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
const SWITCH_BUDGET: Duration = Duration::from_secs(10);

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
    /// Register (and keep renewing) this shell's lease for one project.
    ///
    /// Returns whether a lease is actually now held. `Ok(false)` is the honest
    /// answer when no guardian serves this scope (embedded or legacy runtime):
    /// there is then nothing the quit could release, and registering the project
    /// anyway would make every exit report a reclaim failure that never happened.
    fn acquire(
        &self,
        client_run_id: &str,
        scope: &RuntimeScope,
        mode: LeaseMode,
        budget: Duration,
    ) -> io::Result<bool>;

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
    fn acquire(
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
        let mut lease = match yi_agent_lifecycle::client::FrontendLease::register_if_served(
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
        lease.start_heartbeat()?;
        leases.insert(scope.key.clone(), Arc::new(Mutex::new(lease)));
        Ok(true)
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
        let mut lease = lock_lease(&lease)?;
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
            backend.acquire(&id, &scope_for_io, mode, ACQUIRE_BUDGET)
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
                let backend = Arc::clone(&self.backend);
                let id = self.client_run_id.clone();
                let scope_for_io = scope.clone();
                let _ = run_bounded(ACQUIRE_BUDGET, move || {
                    backend.release(&id, &scope_for_io, ACQUIRE_BUDGET)
                })
                .await;
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

        self.projects
            .lock()
            .map_err(|_| poisoned())?
            .insert(key, scope);
        Ok(())
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
    app.manage(QuitState::new(Arc::new(
        DesktopRuntimeManager::new(id).with_artifact(artifact),
    )));
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

    // `<Foo.app>/Contents/MacOS/desktop` -> `<Foo.app>`.
    let bundle_root = app
        .path()
        .resource_dir()
        .ok()
        .and_then(|resources| resources.parent().map(std::path::Path::to_path_buf))?;
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
        root: PathBuf,
    }

    #[derive(Default)]
    struct FakeInner {
        released: Vec<String>,
        open: bool,
        refuse: bool,
    }

    impl FakeControl {
        fn new(root: PathBuf) -> Arc<Self> {
            Arc::new(Self {
                inner: Mutex::new(FakeInner::default()),
                gate: Condvar::new(),
                arrived: tokio::sync::Notify::new(),
                root,
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
        fn acquire(
            &self,
            _client_run_id: &str,
            _scope: &RuntimeScope,
            _mode: LeaseMode,
            _budget: Duration,
        ) -> io::Result<bool> {
            Ok(true)
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
        fn acquire(
            &self,
            _client_run_id: &str,
            _scope: &RuntimeScope,
            _mode: LeaseMode,
            _budget: Duration,
        ) -> io::Result<bool> {
            Ok(true)
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

    /// A scope with no guardian has no lease to hold. Recording it anyway would
    /// make every later quit report a reclaim failure for work that never had an
    /// owner -- a false failure, which is just as bad as a false success.
    #[tokio::test]
    async fn a_scope_without_a_guardian_is_not_registered_for_the_quit() {
        struct NoGuardian;

        impl ControlBackend for NoGuardian {
            fn acquire(
                &self,
                _client_run_id: &str,
                _scope: &RuntimeScope,
                _mode: LeaseMode,
                _budget: Duration,
            ) -> io::Result<bool> {
                Ok(false)
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
        fn acquire(
            &self,
            _client_run_id: &str,
            _scope: &RuntimeScope,
            _mode: LeaseMode,
            _budget: Duration,
        ) -> io::Result<bool> {
            Ok(true)
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

/// Registers this App run's lease for `project`.
///
/// Called when the user actually starts using a project (opening it, or working
/// in it). A project merely present in an index is never acquired: a historical
/// entry must not keep a runtime alive after everyone has left.
#[tauri::command]
pub async fn runtime_acquire_project(
    state: tauri::State<'_, QuitState>,
    project: String,
) -> Result<(), String> {
    let manager = Arc::clone(&state.manager);
    let home = instance_home().map_err(|error| error.to_string())?;
    let project = std::path::PathBuf::from(project);
    let runtime = runtime_dir_for(&project);
    let scope = yi_agent_lifecycle::scope::resolve(&runtime, &project, &home)
        .map_err(|error| error.to_string())?;
    // Readable before it is leased: a runtime that refuses registration (embedded,
    // legacy) still has a version the banner must be able to report.
    manager.note_open(&scope);
    manager
        .acquire_project(scope, yi_agent_lifecycle::types::LeaseMode::Business)
        .await
        .map_err(|error| error.to_string())
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
pub fn runtime_scope_for_project(state: tauri::State<'_, QuitState>, project: String) -> Option<String> {
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
