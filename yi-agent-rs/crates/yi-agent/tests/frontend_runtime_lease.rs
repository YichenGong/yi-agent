//! Who counts as a frontend of the shared runtime, and what leaving means.
//!
//! The lease exists for one question the business socket cannot answer: is the
//! process that owns this work still there? Two things follow, and both are tested
//! here because a unit test on the lease table cannot see either:
//!
//! * A real `yi-agent run` registers, and **releases on exit** -- the evidence is
//!   the guardian's own reclaim record, not a claim in a comment.
//! * A read-only command is not a frontend. If it registered, a `daemon status`
//!   would keep a runtime alive and its exit would reclaim other people's work.

mod common;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use yi_agent_lifecycle::client::{ControlClient, FrontendLease};
use yi_agent_lifecycle::deadline::Deadline;
use yi_agent_lifecycle::types::{
    ArtifactIdentity, ClientKind, ControlErrorCode, ControlRequest, ControlResponse, LeaseMode,
    RuntimeScope,
};
use yi_agent_supervisors::lifecycle::guardian::{GUARDIAN_SOCKET_FILE, serve};

/// How long a *quitting* frontend has to show up in the guardian's records.
///
/// Deliberately shorter than `LEASE_EXPIRY` (30 s): a reclaim inside this budget
/// can only come from an explicit release, which is what quitting is supposed to
/// do. A longer budget would let the expiry watchdog produce the same record and
/// hide a frontend that never released.
const RELEASE_BUDGET: Duration = Duration::from_secs(12);

/// The plan's structural assertion: an internal worker has no way to become a
/// frontend, not even by naming itself one on the wire.
#[test]
fn internal_worker_is_not_a_frontend_kind() {
    let kinds = [ClientKind::App, ClientKind::Tui, ClientKind::Headless];
    assert_eq!(kinds.len(), 3);
    assert!(
        serde_json::from_str::<ClientKind>("\"Worker\"").is_err(),
        "a worker that deserialized as a client kind could keep a runtime alive forever"
    );
}

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

struct Fixture {
    root: tempfile::TempDir,
    workdir: PathBuf,
    runtime_dir: PathBuf,
    home: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::TempDir::new().expect("tempdir");
        let workdir = root.path().join("repository");
        let runtime_dir = root.path().join("runtime");
        let home = root.path().join("home");
        std::fs::create_dir_all(&workdir).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        for arguments in [
            vec!["init", "-q"],
            vec!["config", "user.name", "Yi"],
            vec!["config", "user.email", "yi@example.test"],
        ] {
            let status = Command::new("git")
                .current_dir(&workdir)
                .args(&arguments)
                .status()
                .expect("run git");
            assert!(status.success(), "git {arguments:?} failed");
        }
        std::fs::write(workdir.join("README.md"), "lease fixture\n").unwrap();
        // The instance home is isolated, so without a catalog the first launch stops
        // on the interactive onboarding prompt and never reaches the runtime. That
        // is correct product behaviour and useless as a test fixture, so the fixture
        // supplies the catalog the prompt would have written, and marks onboarding
        // as dismissed so no test can hang on a wizard.
        let instance = root.path().join("instance/.yi-agent");
        std::fs::create_dir_all(&instance).unwrap();
        std::fs::write(
            instance.join("models.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "models": [{
                    "name": "fixture",
                    "provider": "openai",
                    "api_url": "http://127.0.0.1:9",
                    "model": "gpt-4o",
                    "api_key": "test-key"
                }],
                "default_model": "fixture"
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            instance.join("preferences.json"),
            br#"{"onboarding_dismissed":true}"#,
        )
        .unwrap();
        let _ = Command::new("git")
            .current_dir(&workdir)
            .args(["add", "README.md"])
            .status();
        let _ = Command::new("git")
            .current_dir(&workdir)
            .args(["commit", "-q", "-m", "baseline"])
            .status();
        Self {
            root,
            workdir,
            runtime_dir,
            home,
        }
    }

    fn scope(&self) -> RuntimeScope {
        RuntimeScope {
            key: self.runtime_dir.to_string_lossy().into_owned(),
            runtime_dir: self.runtime_dir.clone(),
            project_key: self.workdir.to_string_lossy().into_owned(),
            instance_home: self.root.path().to_path_buf(),
        }
    }

    fn control_socket(&self) -> PathBuf {
        self.runtime_dir.join(GUARDIAN_SOCKET_FILE)
    }
}

/// A guardian on its own thread, stopped on drop.
struct GuardianFixture {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl GuardianFixture {
    fn start(fixture: &Fixture) -> Self {
        let scope = fixture.scope();
        let socket = fixture.control_socket();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_for_thread = std::sync::Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            let _ = serve(scope, artifact(), move || {
                stop_for_thread.load(std::sync::atomic::Ordering::SeqCst)
            });
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while !socket.exists() {
            assert!(
                Instant::now() < deadline,
                "the guardian never bound its control socket"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        // The socket file exists after `bind`; wait for the serve loop to actually
        // answer, otherwise a client can connect into the gap and be refused.
        let deadline_inner = Deadline::after(Duration::from_secs(10));
        loop {
            if let Ok(mut client) = ControlClient::connect(&socket, &deadline_inner) {
                if client
                    .request(&ControlRequest::Identity, &deadline_inner)
                    .is_ok()
                {
                    break;
                }
            }
            assert!(
                Instant::now() < deadline,
                "the guardian never answered an identity request"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        Self {
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for GuardianFixture {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// One OpenAI-compatible chat completion: the turn ends immediately.
fn spawn_mock_gateway(listener: TcpListener) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut buffer = vec![0u8; 65536];
            let read = stream.read(&mut buffer).unwrap_or(0);
            let body = String::from_utf8_lossy(&buffer[..read]).to_string();
            let request_line = body.lines().next().unwrap_or("");
            // The Responses route is probed first under `api_style: auto`; answer it
            // with a 404 so the client falls back to chat completions instead of
            // parsing a chat-shaped 200 with the wrong parser.
            if request_line.contains("/responses") {
                let _ = stream.write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                continue;
            }
            let mut out = String::from(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            );
            out.push_str("data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n");
            out.push_str("data: {\"choices\":[{\"finish_reason\":\"stop\",\"delta\":{}}]}\n\n");
            out.push_str("data: [DONE]\n\n");
            let _ = stream.write_all(out.as_bytes());
            let _ = stream.flush();
        }
    })
}

fn headless_command(fixture: &Fixture, port: u16, prompt: &str) -> Command {
    let mut command = Command::new(common::yi_agent_bin());
    command
        .arg("--workdir")
        .arg(&fixture.workdir)
        .args(["--provider", "openai"])
        .arg("--api-url")
        .arg(format!("http://127.0.0.1:{port}"))
        .args(["--api-key", "test-key", "--model", "gpt-4o", "--yolo"])
        .args(["run", "--subagents", prompt])
        .env("YI_AGENT_RUNTIME_DIR", &fixture.runtime_dir)
        .env("YI_AGENT_HOME", fixture.root.path().join("instance"))
        .env("HOME", &fixture.home)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// Every reclaim record the guardian has written for this scope.
fn reclaim_records(
    runtime_dir: &Path,
) -> Vec<yi_agent_supervisors::lifecycle::reclaim::PersistedReclaim> {
    let Ok(entries) = std::fs::read_dir(runtime_dir) else {
        return Vec::new();
    };
    let mut records = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !yi_agent_supervisors::lifecycle::reclaim::is_result_file(&path) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Ok(record) = serde_json::from_str::<
            yi_agent_supervisors::lifecycle::reclaim::PersistedReclaim,
        >(&text)
        {
            records.push(record);
        }
    }
    records
}

fn wait_for_reclaim_records(
    runtime_dir: &Path,
    count: usize,
    budget: Duration,
) -> Vec<yi_agent_supervisors::lifecycle::reclaim::PersistedReclaim> {
    let deadline = Instant::now() + budget;
    loop {
        let records = reclaim_records(runtime_dir);
        if records.len() >= count {
            return records;
        }
        assert!(
            Instant::now() < deadline,
            "expected {count} reclaim record(s) within {budget:?}, found {}: {:?}",
            records.len(),
            records
                .iter()
                .map(|record| record.result.client_run_id.clone())
                .collect::<Vec<_>>()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A real `yi-agent run` is a frontend: it registers, and on exit it releases and
/// the guardian proves the release stopped its work.
///
/// The evidence is the guardian's own record. A test that only checked the child's
/// exit code would pass just as happily if the lease had never been registered --
/// and an unregistered client's work is exactly what the guardian reclaims out from
/// under it.
#[test]
fn a_headless_run_registers_and_releases_its_lease() {
    let fixture = Fixture::new();
    let _guardian = GuardianFixture::start(&fixture);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock gateway");
    let port = listener.local_addr().unwrap().port();
    let _gateway = spawn_mock_gateway(listener);

    let output = headless_command(&fixture, port, "say done")
        .output()
        .expect("run yi-agent");
    assert!(
        output.status.success(),
        "the run must finish cleanly: stdout: {}; stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // Well inside the 30 s lease window: passing only because the lease eventually
    // expired would mean the run never released, which is the bug this checks.
    let records = wait_for_reclaim_records(&fixture.runtime_dir, 1, RELEASE_BUDGET);
    let record = records
        .iter()
        .find(|record| record.result.client_run_id.starts_with("run:"))
        .unwrap_or_else(|| {
            panic!(
                "the run never became a client of the runtime: {:?}",
                records
                    .iter()
                    .map(|record| record.result.client_run_id.clone())
                    .collect::<Vec<_>>()
            )
        });
    assert!(
        record.result.stopped,
        "a released lease must prove its work stopped: {:?}",
        record.result.failures
    );
}

/// `daemon status` reads and reports; it must not become a client.
///
/// Checked structurally as well as behaviourally, because "no record was written"
/// is equally true of a command that silently failed before reaching the runtime:
/// the source guard is what rules out a registration added later.
#[test]
fn a_read_only_command_never_registers_a_frontend() {
    let source = include_str!("../src/main.rs");
    let status_path = source
        .split("fn control_daemon_client")
        .nth(1)
        .expect("the read-only daemon client must exist")
        .split("\nfn ")
        .next()
        .unwrap_or_default();
    assert!(
        !status_path.contains("FrontendLease"),
        "a read-only status query that registered a lease would keep a runtime alive \
         and reclaim work on exit: {status_path}"
    );

    let fixture = Fixture::new();
    let _guardian = GuardianFixture::start(&fixture);
    let output = Command::new(common::yi_agent_bin())
        .arg("--workdir")
        .arg(&fixture.workdir)
        .args(["daemon", "status"])
        .env("YI_AGENT_RUNTIME_DIR", &fixture.runtime_dir)
        .env("YI_AGENT_HOME", fixture.root.path().join("instance"))
        .env("HOME", &fixture.home)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run daemon status");

    // No daemon is serving the business socket here, so the command is expected to
    // report that. What matters is that nothing registered.
    assert!(
        !output.status.success(),
        "with no business daemon, status must report failure rather than a lease"
    );
    std::thread::sleep(Duration::from_millis(300));
    let records = reclaim_records(&fixture.runtime_dir);
    assert!(
        records.is_empty(),
        "a read-only command must never create a client whose work is then reclaimed: {:?}",
        records
            .iter()
            .map(|record| record.result.client_run_id.clone())
            .collect::<Vec<_>>()
    );
}

fn ask(socket: &Path, request: &ControlRequest) -> ControlResponse {
    let deadline = Deadline::after(Duration::from_secs(5));
    let mut client = ControlClient::connect(socket, &deadline).expect("connect to guardian");
    client.request(request, &deadline).expect("control request")
}

fn register(socket: &Path, id: &str, kind: ClientKind, mode: LeaseMode) -> FrontendLease {
    let deadline = Deadline::after(Duration::from_secs(5));
    FrontendLease::register(socket, id.to_string(), kind, mode, &deadline).expect("register")
}

/// Two frontends, one exit: the survivor keeps its lease and its admission.
///
/// Getting this wrong looks like success from inside the departing process, which
/// is why the assertion is made from the *survivor's* point of view.
#[test]
fn one_frontend_leaving_does_not_reclaim_anothers_work() {
    let fixture = Fixture::new();
    let _guardian = GuardianFixture::start(&fixture);
    let socket = fixture.control_socket();
    let mut tui = register(&socket, "tui:1", ClientKind::Tui, LeaseMode::Business);
    let headless = register(&socket, "run:1", ClientKind::Headless, LeaseMode::Business);

    let deadline = Deadline::after(Duration::from_secs(5));
    let operation = tui.release(&deadline).expect("release the TUI lease");
    let result = tui
        .await_result(&operation, Duration::from_secs(10))
        .expect("poll the reclaim")
        .expect("the reclaim must finish inside the budget");
    assert!(result.stopped, "{:?}", result.failures);
    assert_eq!(result.client_run_id, "tui:1");

    // The survivor is still a client the guardian answers for.
    match ask(
        &socket,
        &ControlRequest::Heartbeat {
            grant: headless.grant().clone(),
        },
    ) {
        ControlResponse::Ack => {}
        other => panic!("the surviving frontend lost its lease: {other:?}"),
    }
    let generation = format!("guardian:{}", std::process::id());
    let generation = generation.replace(':', "-");
    match ask(
        &socket,
        &ControlRequest::Admission {
            asked: yi_agent_lifecycle::types::ExecutionAdmission {
                grant: headless.grant().clone(),
                generation,
                task: "thread:t".to_string(),
                attempt: "attempt:1".to_string(),
            },
        },
    ) {
        ControlResponse::Ack => {}
        other => panic!("the survivor must still be admitted: {other:?}"),
    }

    // And the departing client is genuinely gone, not merely quiet.
    match ask(
        &socket,
        &ControlRequest::Heartbeat {
            grant: tui.grant().clone(),
        },
    ) {
        ControlResponse::Rejected { code, .. } => {
            assert!(
                matches!(
                    code,
                    ControlErrorCode::StaleEpoch | ControlErrorCode::Unauthenticated
                ),
                "a released lease must be refused, got {code:?}"
            );
        }
        other => panic!("a released lease was still accepted: {other:?}"),
    }
}

/// A real TUI, on a real terminal, that really leaves.
///
/// The TUI is the frontend this whole mechanism exists for, and its exit path has
/// three orderings that a table test cannot see: raw mode is restored, the root is
/// detached, and only then is the lease released. Driving it through a pty is what
/// makes the Ctrl+Q actually reach `run_tui` instead of a closed pipe.
///
/// What is asserted is the guardian's record of a *stopped* reclaim: the TUI must
/// appear as a client and be reclaimed when it quits. A test that only waited for
/// the process to exit would pass even if no lease had ever been registered.
#[test]
fn a_tui_exit_releases_its_lease() {
    let fixture = Fixture::new();
    // `always` pre-seeds the runtime choice, so the TUI does not sit on a modal
    // dialog waiting for input we are not going to send.
    std::fs::create_dir_all(fixture.workdir.join(".yi-agent")).unwrap();
    std::fs::write(
        fixture.workdir.join(".yi-agent/preferences.json"),
        br#"{"subagent_runtime":"always"}"#,
    )
    .unwrap();
    let _guardian = GuardianFixture::start(&fixture);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock gateway");
    let port = listener.local_addr().unwrap().port();
    let _gateway = spawn_mock_gateway(listener);

    let mut terminal = PtyTerminal::spawn(&fixture, port);
    // Wait until the TUI has really brought its runtime up: the lease is what the
    // exit path releases, so leaving before there is anything to release would
    // test nothing.
    terminal.await_runtime_attached(&fixture);
    terminal.quit();

    // The TUI released on its own, inside a budget shorter than its lease window: a
    // reclaim that only appears after 30 s is an expired lease, not a release, and
    // would mean quitting silently relied on the watchdog.
    let records = wait_for_reclaim_records(&fixture.runtime_dir, 1, RELEASE_BUDGET);
    let record = records
        .iter()
        .find(|record| record.result.client_run_id.starts_with("tui:"))
        .unwrap_or_else(|| {
            panic!(
                "a TUI that quit was never a client of the runtime: {:?}",
                records
                    .iter()
                    .map(|record| record.result.client_run_id.clone())
                    .collect::<Vec<_>>()
            )
        });
    assert!(
        record.result.stopped,
        "the TUI's own exit must reclaim its work: {:?}",
        record.result.failures
    );
    terminal.reap();
}

/// A TUI on a pseudo-terminal, owned so it is killed on every path.
struct PtyTerminal {
    child: std::process::Child,
    master: std::os::unix::io::RawFd,
}

impl PtyTerminal {
    fn spawn(fixture: &Fixture, port: u16) -> Self {
        let mut master = 0i32;
        let mut slave = 0i32;
        let mut size = libc::winsize {
            ws_row: 40,
            ws_col: 120,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: both descriptors are written by the call before they are read,
        // and the termios/winsize pointers are the null/sized values `openpty`
        // documents.
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size,
            )
        };
        assert_eq!(rc, 0, "openpty failed: {}", std::io::Error::last_os_error());
        // The child must get the slave as its controlling terminal, or crossterm
        // has no terminal to put into raw mode and `run_tui` fails before the
        // lease exists.
        use std::os::fd::FromRawFd;
        // SAFETY: `openpty` returned both descriptors and hands ownership over.
        let slave_for_child = unsafe { std::os::fd::OwnedFd::from_raw_fd(slave) };
        let master_for_parent = unsafe { std::os::fd::OwnedFd::from_raw_fd(master) };

        let mut command = Command::new(common::yi_agent_bin());
        command
            .arg("--workdir")
            .arg(&fixture.workdir)
            .args(["--provider", "openai"])
            .arg("--api-url")
            .arg(format!("http://127.0.0.1:{port}"))
            .args(["--api-key", "test-key", "--model", "gpt-4o", "--yolo"])
            .env("YI_AGENT_RUNTIME_DIR", &fixture.runtime_dir)
            .env("YI_AGENT_HOME", fixture.root.path().join("instance"))
            .env("HOME", &fixture.home)
            .env("TERM", "xterm-256color")
            .stdin(Stdio::from(slave_for_child))
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = command.spawn().expect("spawn the TUI on a pty");
        use std::os::fd::IntoRawFd;
        let master = master_for_parent.into_raw_fd();
        Self { child, master }
    }

    /// Waits until the TUI has attached its root to the runtime.
    ///
    /// Root attach is the last thing that happens before the lease is registered,
    /// in the same driver iteration, so this is the closest honest readiness signal
    /// a test can get without a "list clients" request the protocol does not have.
    /// The settle wait after it covers the few hundred microseconds in between;
    /// quitting before the attach would only prove the TUI can exit.
    fn await_runtime_attached(&mut self, fixture: &Fixture) {
        let socket = fixture.runtime_dir.join("runtime.sock");
        let deadline = Instant::now() + Duration::from_secs(60);
        while !socket.exists() {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                panic!("the TUI exited before it attached a runtime root");
            }
            assert!(
                Instant::now() < deadline,
                "the TUI never attached a root at {}",
                socket.display()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    /// Sends Ctrl+Q, which is the TUI's unconditional quit.
    fn quit(&mut self) {
        let bytes = [0x11u8];
        // SAFETY: `master` is an open descriptor owned by this guard, and the
        // buffer is alive for the call.
        let written = unsafe {
            libc::write(
                self.master,
                bytes.as_ptr().cast::<libc::c_void>(),
                bytes.len(),
            )
        };
        assert!(written > 0, "could not type into the TUI's terminal");
    }

    fn reap(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for PtyTerminal {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // SAFETY: the descriptor is owned by this guard and closed exactly once.
        unsafe { libc::close(self.master) };
    }
}
