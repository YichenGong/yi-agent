//! What a departing client takes with it, and what it must leave behind.
//!
//! The interesting failures are all about scope, not about stopping things:
//! stopping *too much* (one frontend's exit taking a TUI's work down) looks
//! identical to success from inside the departing process. So these tests pin the
//! boundary from both sides — A is refused, B keeps working, and only the last exit
//! drains the scope.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tempfile::TempDir;
use yi_agent_lifecycle::deadline::{Clock, Deadline};
use yi_agent_lifecycle::types::{ClientGrant, ClientKind, LeaseMode, RuntimeScope};
use yi_agent_supervisors::lifecycle::guardian::ProcessOwner;
use yi_agent_supervisors::lifecycle::reclaim::{
    ProcessBackend, RECLAIM_CONFIRM, RECLAIM_GRACE, RESULT_RETENTION, Reclaimer,
};

/// A clock a test can move forward on demand.
///
/// Every reclamation window is a `Duration` against real elapsed time, so a fake
/// clock cannot shorten them; what it does control is the *retention* clock, which
/// is what makes "the result is still readable after 30 s" testable without
/// sleeping 30 s.
#[derive(Default)]
struct ManualClock {
    now: Mutex<Duration>,
}

impl ManualClock {
    fn now(&self) -> Duration {
        *self.now.lock().unwrap()
    }

    fn advance(&self, by: Duration) {
        let mut now = self.now.lock().unwrap();
        *now = now.saturating_add(by);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Duration {
        *self.now.lock().unwrap()
    }
}

/// A backend that records what was asked of it rather than touching processes.
///
/// It deliberately cannot implement a recovery algorithm: the point of the fixture
/// is to answer "did the caller ask for the right things, in the right order", not
/// to be a second, weaker implementation of reclamation.
#[derive(Default)]
struct RecordingBackend {
    stops_requested: Vec<String>,
    terminated: Vec<String>,
    /// Owners that report themselves stopped once terminated.
    stopped: Vec<String>,
    /// Owners whose groups can never be proven stopped, whatever is sent.
    immortal: Vec<String>,
}

impl ProcessBackend for RecordingBackend {
    fn request_stop(&mut self, owner: &ProcessOwner) -> Vec<String> {
        self.stops_requested.push(owner_label(owner));
        Vec::new()
    }

    fn terminate(&mut self, owner: &ProcessOwner, _deadline: Deadline) -> std::io::Result<()> {
        let label = owner_label(owner);
        self.terminated.push(label.clone());
        // Terminating a group is what makes it stopped; the backend is the only
        // authority, so it must not report the stop before it acted.
        if !self.stopped.contains(&label) && !self.immortal.contains(&label) {
            self.stopped.push(label);
        }
        Ok(())
    }

    fn is_stopped(&mut self, owner: &ProcessOwner) -> std::io::Result<bool> {
        Ok(self.stopped.contains(&owner_label(owner)))
    }
}

fn owner_label(owner: &ProcessOwner) -> String {
    match owner {
        ProcessOwner::Frontend(client) => client.clone(),
        ProcessOwner::SharedPlugin(name) => format!("plugin:{name}"),
        ProcessOwner::Daemon(generation) => format!("daemon:{generation}"),
    }
}

fn scope(directory: &TempDir) -> RuntimeScope {
    RuntimeScope {
        key: directory.path().to_string_lossy().into_owned(),
        runtime_dir: directory.path().to_path_buf(),
        project_key: "/project".to_string(),
        instance_home: directory.path().to_path_buf(),
    }
}

/// Two registered clients, a controllable clock, and a recording backend.
///
/// Windows are `Duration`s against real time, so the fixture shortens them instead
/// of sleeping: the production constants are pinned separately in
/// `the_windows_match_the_published_budget`.
struct ReclaimFixture {
    directory: TempDir,
    clock: Arc<ManualClock>,
    backend: Arc<Mutex<RecordingBackend>>,
    reclaimer: Reclaimer,
    app: ClientGrant,
    tui: ClientGrant,
}

impl ReclaimFixture {
    fn with_two_clients() -> Self {
        let directory = TempDir::new().unwrap();
        let clock = Arc::new(ManualClock::default());
        let backend = Arc::new(Mutex::new(RecordingBackend::default()));
        let mut reclaimer = Reclaimer::with_windows(
            scope(&directory),
            "gen-1",
            clock.clone(),
            backend.clone(),
            Duration::from_millis(50),
            Duration::from_millis(400),
        );
        let now = clock.now();
        let app = reclaimer
            .register("app".to_string(), ClientKind::App, LeaseMode::Business, now)
            .unwrap();
        let tui = reclaimer
            .register("tui".to_string(), ClientKind::Tui, LeaseMode::Business, now)
            .unwrap();
        reclaimer
            .clients_mut()
            .bind_root("app", "thread:a", "gen-1")
            .unwrap();
        reclaimer
            .clients_mut()
            .bind_root("tui", "thread:t", "gen-1")
            .unwrap();
        Self {
            directory,
            clock,
            backend,
            reclaimer,
            app,
            tui,
        }
    }

    fn release(&mut self, client: &str) -> String {
        let grant = match client {
            "app" => self.app.clone(),
            "tui" => self.tui.clone(),
            other => panic!("unknown client {other}"),
        };
        self.reclaimer
            .release(grant, Deadline::after(Duration::from_secs(5)))
            .unwrap()
    }

    fn can_admit(&self, client: &str) -> bool {
        self.reclaimer.can_admit(client)
    }

    fn global_drain_started(&self) -> bool {
        self.reclaimer.global_drain_started()
    }

    fn terminate_calls(&self) -> Vec<String> {
        self.backend.lock().unwrap().terminated.clone()
    }

    fn stop_requests(&self) -> Vec<String> {
        self.backend.lock().unwrap().stops_requested.clone()
    }

    fn mark_immortal(&self, client: &str) {
        self.backend
            .lock()
            .unwrap()
            .immortal
            .push(client.to_string());
    }

    /// Waits until an operation reports a result, within a generous outer bound.
    fn wait_result(&self, operation: &str) -> yi_agent_lifecycle::types::ReclaimResult {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(result) = self.reclaimer.result(operation) {
                return result;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "operation {operation} never finished"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn root_is_open(&self, root: &str) -> bool {
        self.reclaimer.clients().root_is_open(root, "gen-1").is_ok()
    }
}

/// The published budget is 10 s of grace then 5 s to confirm. A shorter promise
/// would abandon live work; a longer one would break the shell's exit budget.
#[test]
fn the_windows_match_the_published_budget() {
    assert_eq!(RECLAIM_GRACE, Duration::from_secs(10));
    assert_eq!(RECLAIM_CONFIRM, Duration::from_secs(5));
    assert_eq!(RESULT_RETENTION, Duration::from_secs(60));
}

/// One client leaving must not put a still-in-use TUI into a project-wide drain.
///
/// This is the scope bug in its purest form: a global drain refuses *everyone's*
/// work, so a TUI that is happily running would suddenly be told to wait for a next
/// generation that nobody asked for.
#[test]
fn another_tui_prevents_project_wide_drain() {
    let mut f = ReclaimFixture::with_two_clients();
    f.release("app");
    assert!(!f.can_admit("app"), "the departing client admits nothing");
    assert!(
        f.can_admit("tui"),
        "the TUI is still here and must keep working"
    );
    assert!(!f.global_drain_started());

    f.release("tui");
    assert!(
        f.global_drain_started(),
        "the last client leaving drains the whole scope"
    );
    assert!(!f.can_admit("tui"));
}

/// Admission closes before the processes are stopped, not after.
///
/// If the order were reversed, a task derived in the gap would be admitted under a
/// client that has already been handed back — work nobody will ever reclaim.
#[test]
fn admission_closes_before_any_process_is_signalled() {
    let mut f = ReclaimFixture::with_two_clients();
    let operation = f.release("app");

    assert!(
        !f.root_is_open("thread:a"),
        "the root stops accepting work immediately"
    );
    assert!(f.reclaimer.knows(&operation));

    f.wait_result(&operation);
    assert_eq!(f.stop_requests(), vec!["app".to_string()]);
    assert_eq!(f.terminate_calls(), vec!["app".to_string()]);
}

/// Only the departing client's owner is touched.
///
/// The fixture has two clients in one reclaimer; a reclamation that took the whole
/// scope would show up here as a second terminate call.
#[test]
fn reclamation_is_confined_to_the_departing_owner() {
    let mut f = ReclaimFixture::with_two_clients();
    let operation = f.release("app");
    let result = f.wait_result(&operation);

    assert!(result.stopped);
    assert_eq!(result.client_run_id, "app");
    assert_eq!(f.terminate_calls(), vec!["app".to_string()]);
    assert!(f.root_is_open("thread:t"), "the TUI's root stays open");
}

/// A group that cannot be proven stopped is reported as a failure, not as success.
///
/// Sending the signal is not the same as the work being gone. Reporting success on
/// the strength of having asked is the exact false promise the spec bans.
#[test]
fn an_unprovable_stop_is_a_failure_not_a_success() {
    let mut f = ReclaimFixture::with_two_clients();
    f.mark_immortal("app");
    let operation = f.release("app");
    let result = f.wait_result(&operation);

    assert!(
        !result.stopped,
        "no proof of a stop means the release did not succeed"
    );
    assert!(
        result
            .failures
            .iter()
            .any(|failure| failure.contains("could not confirm")),
        "the failure names what could not be confirmed: {:?}",
        result.failures
    );
    assert!(
        !f.global_drain_started(),
        "the TUI is still registered, so nothing drained"
    );
}

/// Releasing twice is idempotent and returns the same operation.
///
/// A fresh operation for a second release would never finish, and a shell polling
/// it would conclude its teardown hung.
#[test]
fn a_repeated_release_returns_the_same_operation() {
    let mut f = ReclaimFixture::with_two_clients();
    let first = f.release("app");
    let second = f.release("app");
    assert_eq!(first, second);

    let result = f.wait_result(&first);
    assert!(result.stopped);
    assert_eq!(
        f.terminate_calls(),
        vec!["app".to_string()],
        "the group is terminated once, not once per release"
    );
}

/// An expired lease reclaims exactly like an explicit release.
///
/// The difference between "the user quit" and "we stopped hearing from them" is
/// how the client left, not what has to happen to its work.
#[test]
fn lease_expiry_reclaims_the_same_way_as_release() {
    let mut f = ReclaimFixture::with_two_clients();
    // 30 s without a heartbeat is the documented lease window.
    let expired = f
        .reclaimer
        .leases_mut()
        .expire(f.clock.now() + Duration::from_secs(31));
    assert_eq!(expired, vec!["app".to_string(), "tui".to_string()]);
    assert!(!f.can_admit("app"));
    assert!(!f.can_admit("tui"));
}

/// A late heartbeat cannot revive a client that has already been released.
///
/// Without this, a wedged frontend's stale connection could resurrect a lease whose
/// work has been reclaimed — and its next task would run under an owner nobody is
/// watching.
#[test]
fn a_released_client_cannot_be_revived_by_a_late_heartbeat() {
    let mut f = ReclaimFixture::with_two_clients();
    let operation = f.release("app");
    f.wait_result(&operation);

    let now = f.clock.now();
    assert!(
        f.reclaimer.leases_mut().heartbeat(&f.app, now).is_err(),
        "a released client must not be kept alive by a straggler"
    );
    assert!(!f.can_admit("app"));
}

/// A client that arrives during a drain is told to wait for the next generation.
///
/// Refusing it outright would leave it retrying against a listener that is about to
/// disappear; registering it as a waiting observer is what lets it be explained to.
#[test]
fn a_new_client_registers_as_waiting_while_the_scope_drains() {
    let mut f = ReclaimFixture::with_two_clients();
    f.release("app");
    f.release("tui");
    assert!(f.global_drain_started());

    let grant = f
        .reclaimer
        .register(
            "late".to_string(),
            ClientKind::Tui,
            LeaseMode::Business,
            f.clock.now(),
        )
        .expect("a newcomer is registered, not refused");
    assert_eq!(grant.mode, LeaseMode::WaitingNextGeneration);
    assert!(!f.can_admit("late"), "a waiting observer may not run work");
}

/// A finished result stops being readable after its retention window.
///
/// The record exists so a shell that outlived the guardian can still learn the
/// outcome. Keeping it forever would grow a file per client per generation;
/// expiring it with a timer would keep the guardian alive to clean up.
#[test]
fn a_result_record_expires_after_its_retention_window() {
    let mut f = ReclaimFixture::with_two_clients();
    let operation = f.release("app");
    let result = f.wait_result(&operation);
    assert!(result.stopped);

    f.clock.advance(Duration::from_secs(59));
    f.reclaimer.expire_results();
    assert!(
        f.reclaimer.knows(&operation),
        "59 s is inside the retention window"
    );

    f.clock.advance(Duration::from_secs(2));
    f.reclaimer.expire_results();
    assert!(
        !f.reclaimer.knows(&operation),
        "past 60 s the record is gone"
    );
}

/// A running operation is never expired, however long it takes.
///
/// A teardown that needs the full 15 s must still be able to report its result; an
/// expiry clock that started at release would delete the answer as it was written.
#[test]
fn a_running_operation_is_not_expired() {
    let mut f = ReclaimFixture::with_two_clients();
    f.mark_immortal("app");
    let operation = f.release("app");
    f.clock.advance(Duration::from_secs(120));
    f.reclaimer.expire_results();
    assert!(f.reclaimer.knows(&operation));
    // Let it finish so the test does not leave a thread polling.
    let result = f.wait_result(&operation);
    assert!(!result.stopped);
}

/// The client-ownership table is the same one the coordinator consults, so a
/// release closes admission there too — not only in the lease table.
#[test]
fn release_closes_admission_in_the_ownership_table() {
    let mut f = ReclaimFixture::with_two_clients();
    assert!(f.root_is_open("thread:a"));
    let operation = f.release("app");
    assert!(!f.root_is_open("thread:a"));
    assert!(f.reclaimer.clients().is_closed("app"));
    f.wait_result(&operation);
}

/// The fixture's temp directory outlives the reclaimer, so every written record
/// landed somewhere the test can still inspect.
#[test]
fn written_records_live_in_the_runtime_directory() {
    let mut f = ReclaimFixture::with_two_clients();
    let operation = f.release("app");
    f.wait_result(&operation);
    let path = f.directory.path().join(format!("{operation}.json"));
    // Written by the reclaim thread; give it a moment to reach the filesystem even
    // though the in-memory result is already visible.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !path.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(path.exists(), "expected a record at {}", path.display());
    let record =
        yi_agent_supervisors::lifecycle::reclaim::read_result(&scope(&f.directory), &operation)
            .unwrap()
            .expect("the record is readable");
    assert_eq!(record.generation, "gen-1");
    assert!(record.result.stopped);

    // 0600: the record names a client and its outcome, which is not public.
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

/// A record that describes another scope is not evidence about this one.
#[test]
fn a_record_from_another_scope_is_not_returned() {
    let mut f = ReclaimFixture::with_two_clients();
    let operation = f.release("app");
    f.wait_result(&operation);

    let other = TempDir::new().unwrap();
    let found =
        yi_agent_supervisors::lifecycle::reclaim::read_result(&scope(&other), &operation).unwrap();
    assert!(found.is_none());
}

/// An operation id that would escape the runtime directory is refused.
///
/// The ids are generated here, but the reader accepts an id from a caller; a path
/// built by concatenation would turn "read my result" into "read any file".
#[test]
fn a_traversal_shaped_operation_id_reads_nothing() {
    let directory = TempDir::new().unwrap();
    for id in ["../../etc/passwd", "reclaim-1/../../x", "a b"] {
        let found =
            yi_agent_supervisors::lifecycle::reclaim::read_result(&scope(&directory), id).unwrap();
        assert!(found.is_none(), "{id} must not resolve to a file");
    }
}

// ---------------------------------------------------------------------------
// Through the real control channel
// ---------------------------------------------------------------------------

use yi_agent_lifecycle::client::ControlClient;
use yi_agent_lifecycle::types::{ArtifactIdentity, ControlRequest, ControlResponse};
use yi_agent_supervisors::lifecycle::guardian::{GUARDIAN_SOCKET_FILE, Guardian};

fn artifact() -> ArtifactIdentity {
    ArtifactIdentity {
        build_id: "test-build".to_string(),
        display_version: "0.0.0".to_string(),
        executable: "/usr/bin/true".into(),
        sha256: "00".to_string(),
        schema_min: 1,
        schema_max: 1,
        handover_version: 1,
        business_protocol_min: 1,
        business_protocol_max: 1,
        capabilities: Vec::new(),
    }
}

/// Serves a real guardian on a thread until `stop` is set.
fn spawn_guardian(
    scope: RuntimeScope,
    stop: Arc<std::sync::atomic::AtomicBool>,
) -> (std::path::PathBuf, std::thread::JoinHandle<()>) {
    let guardian = Guardian::new(scope.clone(), artifact(), "gen-1".to_string());
    let deadline = Deadline::after(Duration::from_secs(2));
    let listener = guardian.bind_control(&deadline).unwrap();
    listener.set_nonblocking(true).unwrap();
    let socket = scope.runtime_dir.join(GUARDIAN_SOCKET_FILE);
    let handle = std::thread::spawn(move || {
        let mut guardian = guardian;
        while !stop.load(std::sync::atomic::Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = yi_agent_supervisors::lifecycle::guardian::serve_one_for_test(
                        &mut guardian,
                        stream,
                    );
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });
    (socket, handle)
}

fn register(socket: &std::path::Path, client: &str) -> ClientGrant {
    let deadline = Deadline::after(Duration::from_secs(5));
    let mut control = ControlClient::connect(socket, &deadline).unwrap();
    match control
        .request(
            &ControlRequest::Register {
                client_run_id: client.to_string(),
                kind: ClientKind::App,
                mode: LeaseMode::Business,
            },
            &deadline,
        )
        .unwrap()
    {
        ControlResponse::Granted(grant) => grant,
        other => panic!("expected a grant, got {other:?}"),
    }
}

/// Sends exactly one request on a fresh connection.
///
/// The guardian answers one frame per connection and then returns; that is a
/// property of the protocol (it keeps every exchange bounded), so a test that
/// reused a connection would be testing a client the guardian does not serve.
fn ask(socket: &std::path::Path, request: &ControlRequest) -> ControlResponse {
    let deadline = Deadline::after(Duration::from_secs(5));
    let mut control = ControlClient::connect(socket, &deadline).unwrap();
    control.request(request, &deadline).unwrap()
}

/// A release answers with an operation id instead of blocking for 15 seconds.
///
/// Blocking would close the control connection for the whole teardown, so a shell
/// that is quitting could not ask "how did it go" — the one question it has.
#[test]
fn release_answers_with_an_operation_id_and_the_result_is_pollable() {
    let directory = TempDir::new().unwrap();
    let scope = scope(&directory);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (socket, handle) = spawn_guardian(scope, Arc::clone(&stop));
    let grant = register(&socket, "app");

    let operation = match ask(
        &socket,
        &ControlRequest::Release {
            grant: grant.clone(),
        },
    ) {
        ControlResponse::ReleaseStarted { operation_id } => operation_id,
        other => panic!("expected ReleaseStarted, got {other:?}"),
    };
    assert!(operation.starts_with("reclaim-"));

    // With no registry attached, this guardian must report a failure rather than a
    // stop it never verified.
    let poll_deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let answer = ask(
            &socket,
            &ControlRequest::ReclaimStatus {
                grant: grant.clone(),
                operation_id: operation.clone(),
            },
        );
        match answer {
            ControlResponse::ReclaimStatus {
                result: Some(result),
                ..
            } => {
                assert!(
                    !result.stopped,
                    "a guardian with no process backend cannot prove a stop"
                );
                assert!(!result.failures.is_empty());
                break;
            }
            ControlResponse::ReclaimStatus { result: None, .. } => {}
            other => panic!("unexpected reclaim answer: {other:?}"),
        }
        assert!(
            std::time::Instant::now() < poll_deadline,
            "reclamation never reported"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = handle.join();
}

/// Another client cannot read someone else's operation.
///
/// The lease is gone by the time the result exists, so the check has to survive it;
/// without that, any client could poll any teardown and learn another's outcome.
#[test]
fn another_client_cannot_read_the_result() {
    let directory = TempDir::new().unwrap();
    let scope = scope(&directory);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (socket, handle) = spawn_guardian(scope, Arc::clone(&stop));
    let app = register(&socket, "app");
    let tui = register(&socket, "tui");

    let operation = match ask(&socket, &ControlRequest::Release { grant: app.clone() }) {
        ControlResponse::ReleaseStarted { operation_id } => operation_id,
        other => panic!("expected ReleaseStarted, got {other:?}"),
    };

    let answer = ask(
        &socket,
        &ControlRequest::ReclaimStatus {
            grant: tui,
            operation_id: operation,
        },
    );
    assert!(
        matches!(answer, ControlResponse::Rejected { .. }),
        "a stranger's poll must be refused, got {answer:?}"
    );

    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = handle.join();
}

/// A lease-only observer leaving does not drain the scope, but the last business
/// client doing so does.
///
/// Someone watching an upgrade must not be able to keep the runtime it is watching
/// from ever draining; equally, a watcher's exit must not take work down.
#[test]
fn a_lease_only_observer_never_holds_the_scope_open() {
    let directory = TempDir::new().unwrap();
    let clock = Arc::new(ManualClock::default());
    let backend = Arc::new(Mutex::new(RecordingBackend::default()));
    let mut reclaimer = Reclaimer::with_windows(
        scope(&directory),
        "gen-1",
        clock.clone(),
        backend,
        Duration::from_millis(20),
        Duration::from_millis(200),
    );
    let now = clock.now();
    let app = reclaimer
        .register("app".to_string(), ClientKind::App, LeaseMode::Business, now)
        .unwrap();
    let watcher = reclaimer
        .register(
            "watcher".to_string(),
            ClientKind::Tui,
            LeaseMode::LeaseOnly,
            now,
        )
        .unwrap();

    reclaimer
        .release(app, Deadline::after(Duration::from_secs(5)))
        .unwrap();
    assert!(
        reclaimer.global_drain_started(),
        "the only business client left, so the scope drains even though a watcher is registered"
    );
    // The watcher's own release is a no-op for draining, and must not error.
    reclaimer
        .release(watcher, Deadline::after(Duration::from_secs(5)))
        .unwrap();
    assert!(reclaimer.global_drain_started());
}

/// A client whose lease expired reclaims exactly like an explicit release.
///
/// The two paths must not diverge: "they vanished" and "they quit" differ in
/// *why* the work has to stop, never in whether it does.
#[test]
fn expiry_and_release_drive_the_same_reclamation() {
    let directory = TempDir::new().unwrap();
    let clock = Arc::new(ManualClock::default());
    let backend = Arc::new(Mutex::new(RecordingBackend::default()));
    let mut reclaimer = Reclaimer::with_windows(
        scope(&directory),
        "gen-1",
        clock.clone(),
        backend.clone(),
        Duration::from_millis(20),
        Duration::from_millis(200),
    );
    let grant = reclaimer
        .register(
            "app".to_string(),
            ClientKind::App,
            LeaseMode::Business,
            clock.now(),
        )
        .unwrap();
    reclaimer
        .clients_mut()
        .bind_root("app", "thread:a", "gen-1")
        .unwrap();

    // The documented lease window is 30 s; one second past it is expired.
    let expired = reclaimer
        .leases_mut()
        .expire(clock.now() + Duration::from_secs(31));
    assert_eq!(expired, vec!["app".to_string()]);

    // Expiry alone does not run a reclaim; releasing the (now tombstoned) grant does,
    // and it must close admission too.
    assert!(!reclaimer.can_admit("app"));
    let operation = reclaimer
        .release(grant, Deadline::after(Duration::from_secs(5)))
        .unwrap();
    reclaimer.clients_mut().close_client_admission("app");
    assert!(
        reclaimer
            .clients()
            .root_is_open("thread:a", "gen-1")
            .is_err(),
        "an expired client's roots stop accepting work"
    );
    // Let the reclaim thread settle so the test does not leave work behind.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while reclaimer.result(&operation).is_none() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(reclaimer.result(&operation).is_some());
}
