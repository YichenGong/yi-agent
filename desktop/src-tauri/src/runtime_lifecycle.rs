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
use yi_agent_lifecycle::types::{ClientKind, LeaseMode, ReclaimResult, RuntimeScope};

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
}

/// The real backend: one [`yi_agent_lifecycle::client::FrontendLease`] per
/// project, held here so the heartbeat outlives any single sidecar.
pub struct SocketBackend {
    leases: Mutex<BTreeMap<String, yi_agent_lifecycle::client::FrontendLease>>,
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
            if !existing.is_lost() {
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
        leases.insert(scope.key.clone(), lease);
        Ok(true)
    }

    fn release(
        &self,
        client_run_id: &str,
        scope: &RuntimeScope,
        budget: Duration,
    ) -> io::Result<ReclaimResult> {
        let mut lease = {
            let mut leases = self.leases.lock().map_err(|_| poisoned())?;
            leases.remove(&scope.key)
        };
        let Some(lease) = lease.as_mut() else {
            // Nothing registered for this project: there is nothing to reclaim,
            // and claiming a stop nobody asked for would be a false success.
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no lease was held for {}", scope.key),
            ));
        };
        let deadline = Deadline::after(budget);
        let operation = lease.release(&deadline)?;
        match lease.await_result(&operation, budget)? {
            Some(result) => Ok(result),
            None => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("the reclaim for {client_run_id} was not confirmed within the budget"),
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
        }
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
    app.manage(QuitState::new(Arc::new(DesktopRuntimeManager::new(id))));
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

/// The result of the last quit, for the UI and for support reports.
#[tauri::command]
pub fn runtime_quit_outcome(state: tauri::State<'_, QuitState>) -> serde_json::Value {
    let outcome = state.outcome();
    serde_json::json!({
        "clean": outcome.clean,
        "results": outcome.results,
    })
}
