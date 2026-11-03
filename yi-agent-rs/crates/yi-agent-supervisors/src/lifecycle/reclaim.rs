//! Reclaiming one departed client's work, and draining a scope whose last client
//! has left.
//!
//! Three decisions live here, and conflating them is what produces the bug this
//! module exists for:
//!
//! - **Admission closes before anything else.** The departing client's roots stop
//!   accepting work first, so no task can be derived in the window between
//!   "enumerate the work" and "stop the work". Enumerating first would always be
//!   one step behind.
//! - **Only the last business lease drains the whole scope.** One frontend leaving
//!   must not make a still-in-use TUI report a project-wide drain; that would
//!   refuse *its* work and read as a self-inflicted outage.
//! - **A stop is a proof, not a request.** `request_stop` merely asks. Only the
//!   backend's `is_stopped` (or a group termination confirmed by it) may make a
//!   release report success. Answering "reclaimed" on the strength of having sent
//!   a signal is exactly the false success the spec forbids.
//!
//! The two windows are absolute, not per-object: one grace window for the whole
//! owner and one confirmation window after it, no matter how many roots, tasks or
//! tool groups that owner had. A per-object budget would multiply by the number of
//! clients and silently exceed the 10 + 5 second promise.

use std::collections::BTreeMap;
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use yi_agent_lifecycle::deadline::{Clock, Deadline};
use yi_agent_lifecycle::leases::{LeaseTable, ReleaseOutcome};
use yi_agent_lifecycle::types::{
    ClientGrant, ClientKind, ClientRunId, LeaseMode, OperationId, ReclaimResult, RuntimeScope,
};
use yi_agent_store::lifecycle::RuntimeClients;

use super::guardian::{ProcessOwner, ProcessRegistry};

/// Grace given to a departing client's work to stop on its own before it is
/// terminated.
pub const RECLAIM_GRACE: Duration = Duration::from_secs(10);

/// Confirmation window after the group termination was sent.
pub const RECLAIM_CONFIRM: Duration = Duration::from_secs(5);

/// How long a written result stays readable after the guardian is gone.
///
/// The window exists for one case: a shell that released and then outlived the
/// guardian still wants to know its own teardown finished. It is not a reason to
/// keep the guardian alive, so expiry is evaluated on access rather than by a
/// timer.
pub const RESULT_RETENTION: Duration = Duration::from_secs(60);

/// How often the reclaim thread re-checks whether the owner stopped.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// What a reclaimer may do to one owner's OS resources.
///
/// This is deliberately narrow: ask, terminate, and *answer whether it stopped*.
/// The third method is the one that matters — without it the caller can only
/// observe that it sent a signal.
pub trait ProcessBackend: Send {
    /// Ask the owner's processes to stop on their own.
    ///
    /// Returns the reasons a request could not even be delivered; an empty vector
    /// means "asked", not "stopped".
    fn request_stop(&mut self, owner: &ProcessOwner) -> Vec<String>;

    /// Terminate the owner's process groups outright.
    fn terminate(&mut self, owner: &ProcessOwner, deadline: Deadline) -> io::Result<()>;

    /// Is every group the owner had proven stopped?
    fn is_stopped(&mut self, owner: &ProcessOwner) -> io::Result<bool>;
}

/// The production backend: the guardian's own process registry.
///
/// Shared behind a mutex because the reclaim thread runs while the control loop
/// keeps serving. The registry is the only authority on whether a group stopped,
/// so this adapter must not add a second opinion.
pub struct RegistryBackend {
    registry: Arc<Mutex<ProcessRegistry>>,
}

impl RegistryBackend {
    pub fn new(registry: Arc<Mutex<ProcessRegistry>>) -> Self {
        Self { registry }
    }
}

impl ProcessBackend for RegistryBackend {
    fn request_stop(&mut self, owner: &ProcessOwner) -> Vec<String> {
        // The registry has no separate "ask politely" step: a managed group is
        // either running or it is stopped, and group termination is the only way it
        // learns to stop. Reporting a failure here would duplicate `terminate`.
        let _ = owner;
        Vec::new()
    }

    fn terminate(&mut self, owner: &ProcessOwner, deadline: Deadline) -> io::Result<()> {
        self.registry
            .lock()
            .map_err(|_| io::Error::other("process registry mutex poisoned"))?
            .terminate_owner(owner, deadline)
    }

    fn is_stopped(&mut self, owner: &ProcessOwner) -> io::Result<bool> {
        self.registry
            .lock()
            .map_err(|_| io::Error::other("process registry mutex poisoned"))?
            .owner_is_stopped(owner)
    }
}

/// A backend attached to no process registry.
///
/// It exists so a guardian that was never given a registry cannot silently report a
/// successful reclamation. Answering "stopped" without looking at any process would
/// be the false success this module is built to prevent, so every question fails
/// loudly instead.
pub struct DetachedBackend;

impl ProcessBackend for DetachedBackend {
    fn request_stop(&mut self, _owner: &ProcessOwner) -> Vec<String> {
        vec!["no managed process backend is attached to this guardian".to_string()]
    }

    fn terminate(&mut self, _owner: &ProcessOwner, _deadline: Deadline) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "no managed process backend is attached to this guardian",
        ))
    }

    fn is_stopped(&mut self, _owner: &ProcessOwner) -> io::Result<bool> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "no managed process backend is attached to this guardian",
        ))
    }
}

/// Who may read one finished operation's result.
///
/// A release consumes the lease, so the client can no longer prove itself with it.
/// The spec still grants it a read-only view of *its own* operation for a minute;
/// that needs a check that survives the lease, which is why the credential digest
/// is remembered here rather than the lease.
#[derive(Clone, Debug)]
struct QueryRight {
    client: ClientRunId,
    epoch: u64,
    credential_sha256: String,
}

#[derive(Clone, Debug)]
enum OperationState {
    /// A reclaim thread is running. No timestamp: the retention clock starts when
    /// the operation *finishes*, so a long teardown cannot expire its own record.
    Running,
    Finished {
        started_at: Duration,
        result: ReclaimResult,
    },
}

/// Reclamation for one runtime generation.
///
/// It owns the lease table and the client-ownership table because the three
/// questions it answers — may this client admit work, is this its last client, and
/// has its work stopped — must be decided against one consistent view. Splitting
/// them across objects would let a release observe a lease count that a concurrent
/// registration had already changed.
pub struct Reclaimer {
    scope: RuntimeScope,
    generation: String,
    leases: LeaseTable,
    clients: RuntimeClients,
    backend: Arc<Mutex<dyn ProcessBackend>>,
    clock: Arc<dyn Clock>,
    grace: Duration,
    confirm: Duration,
    operations: Arc<Mutex<BTreeMap<OperationId, OperationState>>>,
    /// The operation a repeated release of the same client maps to. A release is
    /// idempotent, and a second call must be able to poll the same answer instead
    /// of creating an operation that will never finish.
    last_for_client: BTreeMap<ClientRunId, OperationId>,
    /// Read-only query rights for operations whose lease is already gone.
    query_rights: BTreeMap<OperationId, QueryRight>,
    next_operation: u64,
}

impl Reclaimer {
    pub fn new(
        scope: RuntimeScope,
        generation: impl Into<String>,
        clock: Arc<dyn Clock>,
        backend: Arc<Mutex<dyn ProcessBackend>>,
    ) -> Self {
        Self::with_windows(
            scope,
            generation,
            clock,
            backend,
            RECLAIM_GRACE,
            RECLAIM_CONFIRM,
        )
    }

    /// Same reclaimer with explicit windows.
    ///
    /// Production always uses [`RECLAIM_GRACE`] + [`RECLAIM_CONFIRM`]; tests pin
    /// those constants separately and then shorten the windows so they do not sleep
    /// fifteen seconds. The windows are *durations*, not injected time, because the
    /// guarantee is about elapsed time and must hold even if the injected clock is
    /// frozen.
    pub fn with_windows(
        scope: RuntimeScope,
        generation: impl Into<String>,
        clock: Arc<dyn Clock>,
        backend: Arc<Mutex<dyn ProcessBackend>>,
        grace: Duration,
        confirm: Duration,
    ) -> Self {
        Self {
            scope,
            generation: generation.into(),
            leases: LeaseTable::new(),
            clients: RuntimeClients::new(),
            backend,
            clock,
            grace,
            confirm,
            operations: Arc::new(Mutex::new(BTreeMap::new())),
            last_for_client: BTreeMap::new(),
            query_rights: BTreeMap::new(),
            next_operation: 0,
        }
    }

    pub fn scope(&self) -> &RuntimeScope {
        &self.scope
    }

    pub fn generation(&self) -> &str {
        &self.generation
    }

    pub fn leases(&self) -> &LeaseTable {
        &self.leases
    }

    pub fn leases_mut(&mut self) -> &mut LeaseTable {
        &mut self.leases
    }

    pub fn clients(&self) -> &RuntimeClients {
        &self.clients
    }

    pub fn clients_mut(&mut self) -> &mut RuntimeClients {
        &mut self.clients
    }

    /// Whether this client may start new work right now.
    ///
    /// Both halves are needed. A released client is refused by its tombstone even
    /// though no global drain is running, and a client that never released is still
    /// refused once the scope itself is draining.
    pub fn can_admit(&self, client: &str) -> bool {
        !self.clients.in_global_drain() && self.leases.admits_execution(client)
    }

    /// Whether the last business client has left and the whole scope is draining.
    pub fn global_drain_started(&self) -> bool {
        self.clients.in_global_drain()
    }

    /// Whether a reclaim operation is still in progress.
    ///
    /// The guardian uses this to decide when it may exit. Leaving with an operation
    /// in flight is not a cosmetic problem: the reclaim runs on its own thread, so
    /// the process ending cuts it off *after* admission was closed and *before* the
    /// answer was recorded. The departing client then polls a socket that is gone
    /// and reports a failed quit for work that may well have stopped.
    pub fn has_running_operations(&self) -> bool {
        self.operations
            .lock()
            .map(|operations| {
                operations
                    .values()
                    .any(|state| matches!(state, OperationState::Running))
            })
            .unwrap_or(false)
    }

    /// Register a client, downgrading it to a waiting observer when the scope is
    /// already draining.
    ///
    /// It is registered rather than refused so the newcomer can be *told* what is
    /// happening ("wait for the next generation") instead of being left to retry
    /// against a listener that is about to disappear.
    pub fn register(
        &mut self,
        id: ClientRunId,
        kind: ClientKind,
        mode: LeaseMode,
        now: Duration,
    ) -> Result<ClientGrant, String> {
        let mode = if self.clients.in_global_drain() {
            LeaseMode::WaitingNextGeneration
        } else {
            mode
        };
        self.leases.register(id, kind, mode, now)
    }

    /// Start reclaiming one client's work; the returned id is polled with
    /// [`Self::result`].
    ///
    /// Returns as soon as the admission is closed and the operation exists. Holding
    /// the caller for the whole 10 + 5 seconds would make the control connection —
    /// and with it the explain-why path — unavailable exactly when a frontend is
    /// trying to shut down.
    ///
    /// `deadline` bounds **only** that synchronous part. The teardown windows that
    /// the background thread uses are [`RECLAIM_GRACE`] and [`RECLAIM_CONFIRM`], and
    /// they are deliberately not derived from the caller's budget: the caller's
    /// deadline is the control connection's request timeout (2 seconds), so folding
    /// the two together would silently cap a 10-second grace at 2 seconds and
    /// terminate client work while it was still stopping cleanly.
    pub fn release(&mut self, grant: ClientGrant, deadline: Deadline) -> io::Result<OperationId> {
        if deadline.is_expired() {
            // Nothing has changed yet, so refusing costs the client nothing and
            // avoids starting a 15-second teardown it can never observe.
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "the caller's budget was already spent before the release began",
            ));
        }
        let client = match self.leases.release(&grant) {
            Ok(ReleaseOutcome::Reclaim(client)) => client,
            Ok(ReleaseOutcome::AlreadyReleased) => {
                // Idempotent. If the first release is still running, its id is the
                // answer; otherwise the finished record is. A brand-new operation
                // would look like work that never completes.
                if let Some(operation) = self.last_for_client.get(&grant.client_run_id) {
                    return Ok(operation.clone());
                }
                let operation = self.next_id();
                let result = ReclaimResult {
                    client_run_id: grant.client_run_id.clone(),
                    // Nothing was registered under this id in this generation, so
                    // there is no work to prove stopped. Claiming otherwise would
                    // require evidence we do not have.
                    stopped: false,
                    failures: vec![format!(
                        "client {} was not registered in this generation",
                        grant.client_run_id
                    )],
                };
                self.record_finished(&operation, result);
                self.remember_query_right(&operation, &grant);
                self.last_for_client
                    .insert(grant.client_run_id.clone(), operation.clone());
                return Ok(operation);
            }
            Err(reason) => {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, reason));
            }
        };

        let operation = self.begin_client_reclaim(client);
        self.remember_query_right(&operation, &grant);
        Ok(operation)
    }

    /// Reclaims every lease whose window has passed, returning the operations.
    ///
    /// An expired lease is *not* just forgotten: the client stopped renewing, which
    /// means it is gone or wedged, and the work it owns has to be stopped and
    /// proven stopped. Dropping the row alone would leave that work running with
    /// no owner -- alive, unreachable, and never reclaimed -- which is exactly the
    /// leak leases exist to prevent.
    ///
    /// No query right is recorded: there is nobody to poll. The operation still
    /// exists, so an operator or a late-returning client can see the outcome
    /// instead of a silent disappearance.
    pub fn expire_leases(&mut self, now: Duration) -> Vec<OperationId> {
        let expired = self.leases.expire(now);
        let mut operations = Vec::with_capacity(expired.len());
        for client in expired {
            operations.push(self.begin_client_reclaim(client));
        }
        if !operations.is_empty() {
            self.expire_results();
        }
        operations
    }

    /// Starts the teardown of one client's work and returns its operation id.
    ///
    /// Shared by release and expiry so the two paths cannot drift: both close
    /// admission first, both drain the scope only when no business client is left,
    /// and both verify the stop rather than assume it.
    fn begin_client_reclaim(&mut self, client: ClientRunId) -> OperationId {
        // Admission closes first, and it closes for *this* client only unless it was
        // the last one. `close_client_admission` is idempotent, so a retried release
        // cannot reopen or duplicate history.
        self.clients.close_client_admission(&client);
        if self.leases.business_active_count() == 0 {
            // Lease-only observers do not keep a scope alive: refusing to drain
            // because someone is watching the upgrade would hang the teardown they
            // are watching for.
            self.clients.begin_global_drain();
        }

        let operation = self.next_id();
        self.last_for_client
            .insert(client.clone(), operation.clone());
        self.record_running(&operation);

        let backend = Arc::clone(&self.backend);
        let operations = Arc::clone(&self.operations);
        let scope = self.scope.clone();
        let generation = self.generation.clone();
        let owner = ProcessOwner::frontend(client.clone());
        // The published windows, never the caller's request budget.
        let grace = self.grace;
        let confirm = self.confirm;
        let started = self.clock.now();
        let operation_for_thread = operation.clone();

        std::thread::spawn(move || {
            let result = reclaim_owner(&backend, &owner, &client, grace, confirm);
            // Persist *before* publishing "finished". The guardian decides it may exit
            // by asking whether any operation is still running, so publishing first
            // opened a window in which the process could leave between "no longer
            // running" and "answer on disk" -- and the client that asked for the
            // teardown would then find neither. Losing the file is not a reason to
            // report a different outcome, so this stays best effort.
            let _ = write_result(&scope, &generation, &operation_for_thread, &result);
            if let Ok(mut operations) = operations.lock()
                && let Some(state) = operations.get_mut(&operation_for_thread)
            {
                *state = OperationState::Finished {
                    started_at: started,
                    result,
                };
            }
        });

        operation
    }

    /// The result of one operation, or `None` while it is still running.
    pub fn result(&self, operation: &str) -> Option<ReclaimResult> {
        let operations = self.operations.lock().ok()?;
        match operations.get(operation)? {
            OperationState::Finished { result, .. } => Some(result.clone()),
            OperationState::Running => None,
        }
    }

    /// Reads one operation's result, proving the caller is the client that started
    /// it.
    ///
    /// `Ok(None)` means "yours, still running"; an error means "not yours" or "no
    /// such operation". Collapsing those into one `None` would let a client poll
    /// another client's teardown and read its outcome.
    pub fn query(
        &self,
        operation: &str,
        grant: &ClientGrant,
    ) -> Result<Option<ReclaimResult>, String> {
        let right = self
            .query_rights
            .get(operation)
            .ok_or_else(|| format!("operation {operation} is not known here"))?;
        if right.client != grant.client_run_id
            || right.epoch != grant.epoch
            || right.credential_sha256 != credential_digest(&grant.credential)
        {
            return Err(format!("operation {operation} belongs to another client"));
        }
        Ok(self.result(operation))
    }

    fn remember_query_right(&mut self, operation: &str, grant: &ClientGrant) {
        self.query_rights.insert(
            operation.to_string(),
            QueryRight {
                client: grant.client_run_id.clone(),
                epoch: grant.epoch,
                credential_sha256: credential_digest(&grant.credential),
            },
        );
    }

    /// Whether an operation id is known at all (running or finished).
    ///
    /// The distinction matters on the wire: "running" and "never heard of it" must
    /// not both answer `None` without saying which.
    pub fn knows(&self, operation: &str) -> bool {
        self.operations
            .lock()
            .map(|operations| operations.contains_key(operation))
            .unwrap_or(false)
    }

    /// Drop finished operations older than [`RESULT_RETENTION`].
    ///
    /// Called opportunistically from a controlled access, never by a timer: nothing
    /// may keep the guardian alive to clean up after itself.
    pub fn expire_results(&mut self) {
        let now = self.clock.now();
        let Ok(operations) = self.operations.lock().map(|ops| ops.clone()) else {
            return;
        };
        let expired: Vec<OperationId> = operations
            .iter()
            .filter(|(_, state)| match state {
                OperationState::Finished { started_at, .. } => {
                    now.saturating_sub(*started_at) >= RESULT_RETENTION
                }
                OperationState::Running => false,
            })
            .map(|(operation, _)| operation.clone())
            .collect();
        if expired.is_empty() {
            return;
        }
        let Ok(mut operations) = self.operations.lock() else {
            return;
        };
        for operation in &expired {
            operations.remove(operation);
            if let Some(path) = result_path(&self.scope, operation) {
                let _ = std::fs::remove_file(path);
            }
        }
        self.last_for_client
            .retain(|_, operation| !expired.contains(operation));
        for operation in &expired {
            self.query_rights.remove(operation);
        }
    }

    fn next_id(&mut self) -> OperationId {
        self.next_operation += 1;
        format!(
            "reclaim-{}-{}-{}",
            self.generation,
            std::process::id(),
            self.next_operation
        )
    }

    fn record_running(&mut self, operation: &str) {
        if let Ok(mut operations) = self.operations.lock() {
            operations.insert(operation.to_string(), OperationState::Running);
        }
    }

    fn record_finished(&mut self, operation: &str, result: ReclaimResult) {
        let started_at = self.clock.now();
        if let Ok(mut operations) = self.operations.lock() {
            operations.insert(
                operation.to_string(),
                OperationState::Finished { started_at, result },
            );
        }
    }
}

/// Stop one owner's work within the two windows.
///
/// The graceful window is shared by every root, task and tool group the owner had:
/// the promise is about the client, not about each of its objects.
fn reclaim_owner(
    backend: &Arc<Mutex<dyn ProcessBackend>>,
    owner: &ProcessOwner,
    client: &str,
    grace: Duration,
    confirm: Duration,
) -> ReclaimResult {
    let mut failures = match backend.lock() {
        Ok(mut backend) => backend.request_stop(owner),
        Err(_) => vec!["process backend is unavailable".to_string()],
    };

    let graceful = Deadline::after(grace);
    if !wait_until_stopped(backend, owner, &graceful) {
        // The graceful phase is over. Terminating the group is the escalation, and
        // it is bounded by the confirmation window rather than by a fresh grace.
        let escalating = Deadline::after(confirm);
        if let Ok(mut backend) = backend.lock() {
            if let Err(error) = backend.terminate(owner, escalating.clone()) {
                failures.push(format!("terminate failed: {error}"));
            }
        } else {
            failures.push("process backend is unavailable".to_string());
        }
        if !wait_until_stopped(backend, owner, &escalating) {
            failures.push(format!(
                "could not confirm that client {client}'s work stopped within {}s",
                grace.saturating_add(confirm).as_secs()
            ));
        }
    }

    ReclaimResult {
        client_run_id: client.to_string(),
        stopped: failures.is_empty(),
        failures,
    }
}

fn wait_until_stopped(
    backend: &Arc<Mutex<dyn ProcessBackend>>,
    owner: &ProcessOwner,
    deadline: &Deadline,
) -> bool {
    loop {
        match backend.lock() {
            Ok(mut backend) => match backend.is_stopped(owner) {
                Ok(true) => return true,
                Ok(false) => {}
                // A backend that cannot answer is not evidence of a stop.
                Err(_) => return false,
            },
            Err(_) => return false,
        }
        let remaining = deadline.remaining();
        if remaining.is_zero() {
            return false;
        }
        std::thread::sleep(remaining.min(POLL_INTERVAL));
    }
}

/// Where one operation's result is written for a shell that outlives us.
fn result_path(scope: &RuntimeScope, operation: &str) -> Option<PathBuf> {
    // The id is generated here, but a path built from caller-supplied text is a
    // traversal waiting to happen. Accepting only the shape we emit keeps that
    // impossible without trusting the caller.
    if !operation
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || character == '-' || character == '_')
    {
        return None;
    }
    Some(scope.runtime_dir.join(format!("{operation}.json")))
}

/// Writes one result atomically at mode 0600.
///
/// Atomic because a half-written file would read as a corrupt record at exactly the
/// moment someone is trying to learn whether their teardown finished. It carries no
/// credential — only the client id, whether it stopped, and why not.
fn write_result(
    scope: &RuntimeScope,
    generation: &str,
    operation: &str,
    result: &ReclaimResult,
) -> io::Result<()> {
    let Some(path) = result_path(scope, operation) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "operation id is not path-safe",
        ));
    };
    std::fs::create_dir_all(&scope.runtime_dir)?;
    let record = PersistedReclaim {
        scope: scope.key.clone(),
        generation: generation.to_string(),
        operation: operation.to_string(),
        result: result.clone(),
    };
    let bytes = serde_json::to_vec_pretty(&record)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let temporary = path.with_extension("json.tmp");
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&temporary, &path)?;
    // Rename preserves the mode, but a pre-existing temp file may have been wider.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// What is written to disk for one operation.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PersistedReclaim {
    pub scope: String,
    pub generation: String,
    pub operation: OperationId,
    pub result: ReclaimResult,
}

/// Reads a written result, if it exists and has not outlived its retention.
///
/// The scope is checked against the file's own contents: a file whose name matches
/// but which describes another scope is not evidence about this one.
pub fn read_result(scope: &RuntimeScope, operation: &str) -> io::Result<Option<PersistedReclaim>> {
    let Some(path) = result_path(scope, operation) else {
        return Ok(None);
    };
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let record: PersistedReclaim = serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if record.scope != scope.key {
        return Ok(None);
    }
    if let Ok(metadata) = std::fs::metadata(&path)
        && let Ok(modified) = metadata.modified()
        && let Ok(age) = modified.elapsed()
        && age >= RESULT_RETENTION
    {
        // Expiry is evaluated on access, never by a timer that would
        // keep the guardian alive to clean up.
        let _ = std::fs::remove_file(&path);
        return Ok(None);
    }
    Ok(Some(record))
}

/// Whether `path` is one of the result files this module writes.
pub fn is_result_file(path: &Path) -> bool {
    path.extension().and_then(|extension| extension.to_str()) == Some("json")
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("reclaim-"))
}

/// One-way digest of a lease credential.
///
/// The query right outlives the lease, so it cannot be "the credential matches" —
/// that would require keeping a secret around for a minute. A digest answers the
/// same question without retaining anything that can be replayed.
fn credential_digest(credential: &str) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(credential.as_bytes());
    format!("{:x}", hasher.finalize())
}
