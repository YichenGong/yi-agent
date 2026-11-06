//! Real processes, real entry points, one guardian.
//!
//! Every other managed-runtime test in this repository either injects the runtime
//! or drives the lease table directly. Both are fine for a module, and both are
//! unable to answer the question this file exists for: *does a user who launches
//! the product get the managed lifecycle?* That answer depends on the wiring -- the
//! frontend has to install its identity, ask the right policy, register a real
//! lease, get a root authorized, and open the managed channel, in that order, in
//! each of the three shipped entry points.
//!
//! So the fixture here starts the actual `yi-agent` binary:
//!
//! * `connect_tui` runs the default TUI entry on a pty, exactly as a terminal does;
//! * `connect_app` runs `app-server --listen stdio://` with the same private
//!   bootstrap frame the desktop shell writes, then `_shell/prepare`;
//! * `connect_headless` runs `run`.
//!
//! No test here calls `FrontendLease::register` to stand in for a client. A lease a
//! test registered proves the lease table works; it says nothing about whether an
//! entry point registers one, which is the whole subject.
//!
//! Ownership rules, because a leaked child outlives the test and poisons the next:
//!
//! * every child is recorded with its *start identity* (pid plus start time), not
//!   just a pid -- pids are recycled and "alive" would then be a question about an
//!   unrelated process;
//! * cleanup stops parents before their supervised children, and only handles this
//!   fixture opened;
//! * nothing is killed by name.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

use yi_agent_lifecycle::deadline::Deadline;
use yi_agent_lifecycle::types::ArtifactIdentity;

/// A managed start is allowed to take a while; it is not allowed to hang.
const START_BUDGET: Duration = Duration::from_secs(90);

/// A quitting client must release inside this budget. The lease itself expires at
/// 30 s, so a longer budget would let expiry produce the same observation and hide
/// a release that never happened.
const RELEASE_BUDGET: Duration = Duration::from_secs(15);

/// One isolated instance: a project, an instance home, a runtime directory, a fake
/// `$HOME`, and the process handles this test opened.
struct IsolatedRuntimeFixture {
    /// Held for the whole test: dropping it removes the project and the runtime.
    #[allow(dead_code)]
    root: tempfile::TempDir,
    workdir: PathBuf,
    runtime_dir: PathBuf,
    instance_home: PathBuf,
    home: PathBuf,
    artifact: ArtifactIdentity,
    /// `(start identity, how to stop it)` for everything this fixture started.
    ///
    /// The start identity is captured at spawn. `same_process_alive` compares it
    /// against the process table, which is what makes "the runtime stopped this
    /// work" a claim about the process that was started rather than about whatever
    /// holds its old pid now.
    owned: Vec<OwnedProcess>,
}

/// A process this fixture started, identified by pid *and* start time.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StartIdentity {
    pid: i32,
    /// Kernel start time in clock ticks since boot (`ps -o lstart=` is textual and
    /// locale dependent; `kinfo_proc` is what `ps` derives it from).
    started: String,
}

#[derive(Debug)]
struct OwnedProcess {
    identity: StartIdentity,
    label: String,
}

impl IsolatedRuntimeFixture {
    /// A fixture with *no* bring-up environment variable set.
    ///
    /// That is deliberate for the acceptance question: a user does not set one, so
    /// a lifecycle that only appears when `YI_AGENT_BRINGUP=managed` is present is
    /// not the lifecycle that ships.
    fn start_without_bringup_env() -> anyhow::Result<Self> {
        let root = tempfile::TempDir::new()?;
        let workdir = root.path().join("repository");
        let runtime_dir = root.path().join("runtime");
        let instance_home = root.path().join("instance");
        let home = root.path().join("home");
        std::fs::create_dir_all(&workdir)?;
        std::fs::create_dir_all(&home)?;
        std::fs::create_dir_all(instance_home.join(".yi-agent"))?;

        for arguments in [
            vec!["init", "-q"],
            vec!["config", "user.name", "Yi"],
            vec!["config", "user.email", "yi@example.test"],
        ] {
            let status = Command::new("git")
                .current_dir(&workdir)
                .args(&arguments)
                .status()?;
            anyhow::ensure!(status.success(), "git {arguments:?} failed");
        }
        std::fs::write(
            workdir.join("README.md"),
            "production integration fixture\n",
        )?;
        for arguments in [
            vec!["add", "README.md"],
            vec!["commit", "-q", "-m", "baseline"],
        ] {
            let _ = Command::new("git")
                .current_dir(&workdir)
                .args(&arguments)
                .status();
        }

        // Without a catalog the first launch stops on the interactive onboarding
        // prompt and never reaches the runtime, so this file would be testing the
        // wizard. Supplying what the prompt would have written is what makes it a
        // runtime fixture.
        let agent_root = instance_home.join(".yi-agent");
        std::fs::write(
            agent_root.join("models.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "models": [{
                    "name": "fixture",
                    "provider": "openai",
                    // Nothing listens here on purpose: these tests never need an
                    // answer, only a runtime that stays alive while it is asked.
                    "api_url": "http://127.0.0.1:9",
                    "model": "gpt-4o",
                    "api_key": "test-key"
                }],
                "default_model": "fixture"
            }))?,
        )?;
        std::fs::write(
            agent_root.join("preferences.json"),
            br#"{"onboarding_dismissed":true}"#,
        )?;
        // The TUI resolves the *project's* runtime choice from the project-local
        // preferences, not the instance one. Without this it opens a modal asking
        // whether to enable the runtime and never reaches the lease -- the test
        // would then be measuring a dialog, not the entry point.
        std::fs::create_dir_all(workdir.join(".yi-agent"))?;
        std::fs::write(
            workdir.join(".yi-agent/preferences.json"),
            br#"{"subagent_runtime":"always"}"#,
        )?;

        let artifact = artifact_of_this_build()?;
        Ok(Self {
            root,
            workdir,
            runtime_dir,
            instance_home,
            home,
            artifact,
            owned: Vec::new(),
        })
    }

    fn scope(&self) -> yi_agent_lifecycle::types::RuntimeScope {
        yi_agent_lifecycle::types::RuntimeScope {
            key: self.runtime_dir.to_string_lossy().into_owned(),
            runtime_dir: self.runtime_dir.clone(),
            project_key: self.workdir.to_string_lossy().into_owned(),
            instance_home: self.instance_home.clone(),
        }
    }

    /// The environment every child gets.
    ///
    /// `YI_AGENT_BRINGUP` is absent on purpose; a test that needs the managed policy
    /// makes the entry point select it, which is the property under test.
    fn apply_env(&self, command: &mut Command) {
        command
            .env("YI_AGENT_RUNTIME_DIR", &self.runtime_dir)
            .env("YI_AGENT_HOME", &self.instance_home)
            .env("HOME", &self.home);
        // Deliberately *not* `YI_AGENT_MODEL`: a bare model override is rejected as
        // incomplete (it needs provider and key too), so setting it would end the
        // process before it reached the runtime and the failure would look like a
        // wiring bug. The catalog above is what selects the model.
    }

    /// Records a child by start identity before returning it.
    fn own(&mut self, child: &Child, label: impl Into<String>) -> anyhow::Result<()> {
        let label = label.into();
        let identity = start_identity(child.id() as i32)
            .ok_or_else(|| anyhow::anyhow!("{label} exited before its start time was read"))?;
        self.owned.push(OwnedProcess { identity, label });
        Ok(())
    }

    /// Whether *the process this fixture started* is still the one running.
    ///
    /// Not `kill(pid, 0)`: a recycled pid would answer "alive" about an unrelated
    /// process, and this is the assertion that decides whether work was reclaimed.
    fn same_process_alive(&self, identity: &StartIdentity) -> bool {
        start_identity(identity.pid).is_some_and(|current| current == *identity)
    }

    fn artifact(&self) -> &ArtifactIdentity {
        &self.artifact
    }

    /// Real stop proof: the process tree is gone, the runtime lock is free, and the
    /// service does not answer.
    ///
    /// The three together are the point. A missing socket alone proves nothing: an
    /// unlinked socket file also disappears when a process merely closes it, and a
    /// lock file left on disk says nothing about whether anyone holds it.
    fn assert_runtime_stopped(&mut self, budget: Duration) -> anyhow::Result<()> {
        let deadline = Instant::now() + budget;
        loop {
            let tree_gone = self
                .owned
                .iter()
                .all(|owned| !self.same_process_alive(&owned.identity));
            let lock_free = runtime_lock_is_free(&self.runtime_dir);
            let service_quiet = !service_answers(&self.runtime_dir);
            if tree_gone && lock_free && service_quiet {
                return Ok(());
            }
            if Instant::now() >= deadline {
                let alive = self
                    .owned
                    .iter()
                    .filter(|owned| self.same_process_alive(&owned.identity))
                    .map(|owned| format!("{}({})", owned.label, owned.identity.pid))
                    .collect::<Vec<_>>();
                anyhow::bail!(
                    "the runtime did not stop: tree_gone={tree_gone} ({alive:?}), \
                     lock_free={lock_free}, service_quiet={service_quiet}"
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for IsolatedRuntimeFixture {
    fn drop(&mut self) {
        // Parents before children: stopping a supervised child first would make the
        // still-running supervisor look broken and, worse, could race a restart.
        for owned in self.owned.iter().rev() {
            if self.same_process_alive(&owned.identity) {
                unsafe { libc::kill(owned.identity.pid, libc::SIGKILL) };
            }
        }
    }
}

/// The identity of the binary this test was built with.
///
/// Read from the binary rather than hand-written: a fixture that invented a build id
/// would make every comparison below a comparison of two fictional values.
fn artifact_of_this_build() -> anyhow::Result<ArtifactIdentity> {
    let output = Command::new(common::yi_agent_bin())
        .args(["internal-runtime", "identity", "--json"])
        .stdin(Stdio::null())
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "identity failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let identity: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    Ok(ArtifactIdentity {
        build_id: identity["build_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("identity has no build_id"))?
            .to_string(),
        display_version: identity["display_version"]
            .as_str()
            .unwrap_or("unknown")
            .to_string(),
        executable: identity["executable"]
            .as_str()
            .map(PathBuf::from)
            .ok_or_else(|| anyhow::anyhow!("identity has no executable"))?,
        sha256: identity["sha256"].as_str().unwrap_or_default().to_string(),
        schema_min: identity["schema_min"].as_u64().unwrap_or(1) as u32,
        schema_max: identity["schema_max"].as_u64().unwrap_or(1) as u32,
        handover_version: identity["handover_version"].as_u64().unwrap_or(1) as u16,
        business_protocol_min: identity["business_protocol_min"].as_u64().unwrap_or(1) as u32,
        business_protocol_max: identity["business_protocol_max"].as_u64().unwrap_or(1) as u32,
        capabilities: identity["capabilities"]
            .as_array()
            .map(|values| {
                values
                    .iter()
                    .filter_map(|value| value.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// A pid together with the kernel's start time for it, or `None` if it is gone.
#[cfg(target_os = "macos")]
fn start_identity(pid: i32) -> Option<StartIdentity> {
    let output = Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let started = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if started.is_empty() {
        return None;
    }
    Some(StartIdentity { pid, started })
}

#[cfg(target_os = "linux")]
fn start_identity(pid: i32) -> Option<StartIdentity> {
    // Field 22 of `/proc/<pid>/stat` is the start time in clock ticks. The comm field
    // can contain spaces, so split after the closing parenthesis.
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = stat.rsplit_once(')')?.1;
    let started = rest.split_whitespace().nth(19)?.to_string();
    Some(StartIdentity { pid, started })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn start_identity(_pid: i32) -> Option<StartIdentity> {
    None
}

/// Whether nobody holds the runtime's exclusive instance lock.
///
/// The lock is an advisory `flock` on `runtime.lock`; a probe that *acquires* it and
/// drops it answers the question without disturbing a runtime that does not hold it.
fn runtime_lock_is_free(runtime_dir: &Path) -> bool {
    use std::os::fd::AsRawFd;
    let path = runtime_dir.join("runtime.lock");
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
    else {
        // No runtime directory at all is not "the lock is held".
        return !runtime_dir.exists() || !path.exists();
    };
    let outcome = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if outcome == 0 {
        // Releasing is closing; do it explicitly so the intent is visible.
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        return true;
    }
    false
}

/// Whether the runtime's control service answers.
///
/// A socket file on disk is not a service. Connecting is the honest question, and a
/// connect that is refused means the runtime is not serving.
fn service_answers(runtime_dir: &Path) -> bool {
    let socket = runtime_dir.join("runtime.sock");
    if !socket.exists() {
        return false;
    }
    let deadline = Deadline::after(Duration::from_millis(500));
    yi_agent_lifecycle::client::ControlClient::connect(&socket, &deadline).is_ok()
}

/// Reads the startup trace the managed gate writes, when the entry point emits one.
fn startup_trace(fixture: &IsolatedRuntimeFixture) -> String {
    std::fs::read_to_string(fixture.root.path().join("startup-trace.log")).unwrap_or_default()
}

/// The managed startup stages, in the order the plan fixes.
const MANAGED_STAGES: [&str; 8] = [
    "prepare",
    "register_business",
    "heartbeat",
    "authorize_root",
    "attach_root",
    "bind_driver",
    "open_managed_channel",
    "admit_turn",
];

/// Every persisted reclaim record the guardian wrote for this scope.
///
/// Read as the type the guardian writes, so a record that is merely *present* but
/// unreadable cannot be counted as evidence.
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

/// Waits until a reclaim record whose client run id starts with `prefix` appears.
///
/// Returns `None` on timeout rather than panicking, so a caller can report *why* it
/// is not evidence: "no record" and "a record that says it failed" are different
/// findings and must not collapse into one message.
fn wait_for_reclaim(
    runtime_dir: &Path,
    prefix: &str,
    budget: Duration,
) -> Option<yi_agent_supervisors::lifecycle::reclaim::PersistedReclaim> {
    let deadline = Instant::now() + budget;
    loop {
        if let Some(record) = reclaim_records(runtime_dir)
            .into_iter()
            .find(|record| record.result.client_run_id.starts_with(prefix))
        {
            return Some(record);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Waits until the guardian's journal lists two *distinct* live frontend owners.
///
/// This is the condition the "one client's exit leaves the other's work alone" claim
/// actually depends on: if the guardian has not journaled the run's process yet, a
/// reclaim cannot know it is somebody else's. Asserting the fact beats sleeping a
/// constant -- a constant that hides a slow machine makes every other case slower,
/// and one that does not hides a real ordering bug.
fn await_two_owners_in_journal(runtime_dir: &Path, budget: Duration) -> anyhow::Result<()> {
    let deadline = Instant::now() + budget;
    loop {
        let owners = live_frontend_owners(runtime_dir);
        if owners.len() >= 2 {
            return Ok(());
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "the guardian never journaled two live frontend owners (saw {owners:?})"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// The distinct frontend owners whose processes the journal still calls running.
fn live_frontend_owners(runtime_dir: &Path) -> Vec<String> {
    let mut owners: Vec<String> = journal_records(runtime_dir)
        .iter()
        .filter(|record| record.get("state").and_then(|v| v.as_str()) == Some("running"))
        .filter_map(|record| {
            record
                .get("owner")
                .and_then(|owner| owner.get("frontend"))
                .and_then(|id| id.as_str())
                .map(str::to_owned)
        })
        .collect();
    owners.sort();
    owners.dedup();
    owners
}

/// The guardian's process journal, as the values it wrote.
fn journal_records(runtime_dir: &Path) -> Vec<serde_json::Value> {
    let path = runtime_dir.join(yi_agent_supervisors::lifecycle::guardian::JOURNAL_FILE);
    let Ok(bytes) = std::fs::read(&path) else {
        return Vec::new();
    };
    if bytes.is_empty() {
        return Vec::new();
    }
    serde_json::from_slice(&bytes).unwrap_or_else(|error| {
        panic!(
            "the guardian's process journal {} is unreadable: {error}",
            path.display()
        )
    })
}

/// One OpenAI-compatible completion that ends immediately.
fn spawn_mock_gateway(listener: std::net::TcpListener) -> std::thread::JoinHandle<()> {
    use std::io::Read as _;
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut buffer = vec![0u8; 65536];
            let read = stream.read(&mut buffer).unwrap_or(0);
            let body = String::from_utf8_lossy(&buffer[..read]).to_string();
            if body.lines().next().unwrap_or("").contains("/responses") {
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

// ---------------------------------------------------------------------------
// Private management channel, driven the way the shell drives it.
// ---------------------------------------------------------------------------

/// An `app-server --listen stdio://` child speaking the private management channel.
///
/// This is the desktop's half of the contract, not a stand-in for it: the flag, the
/// `YI_AGENT_LOCAL_MANAGEMENT` marker and the bootstrap frame are exactly what
/// `desktop/src-tauri/src/bridge.rs` writes. A test that skipped them would be
/// driving a channel production never opens.
struct Sidecar {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    secret: String,
    epoch: String,
}

impl Sidecar {
    fn spawn(fixture: &mut IsolatedRuntimeFixture) -> anyhow::Result<Self> {
        let secret = format!("secret-{}", uuid_like());
        let epoch = format!("epoch-{}", uuid_like());
        let mut command = Command::new(common::yi_agent_bin());
        fixture.apply_env(&mut command);
        command
            .args(["app-server", "--listen", "stdio://"])
            .env("YI_AGENT_LOCAL_MANAGEMENT", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn()?;
        fixture.own(&child, "sidecar")?;
        let stdin = child.stdin.take().expect("sidecar stdin");
        let stdout = BufReader::new(child.stdout.take().expect("sidecar stdout"));
        let mut sidecar = Self {
            child,
            stdin,
            stdout,
            secret,
            epoch,
        };
        // The first frame is the credential, and it has no `id`: it is a session
        // identity, not a request.
        sidecar.write(&serde_json::json!({
            "secret": sidecar.secret,
            "sidecar_epoch": sidecar.epoch,
        }))?;
        let ack = sidecar.read_frame(Duration::from_secs(30))?;
        anyhow::ensure!(
            ack.get("bootstrap").and_then(|v| v.as_str()) == Some("accepted"),
            "the sidecar refused the bootstrap: {ack}"
        );
        Ok(sidecar)
    }

    fn write(&mut self, frame: &serde_json::Value) -> anyhow::Result<()> {
        writeln!(self.stdin, "{frame}")?;
        self.stdin.flush()?;
        Ok(())
    }

    fn read_frame(&mut self, budget: Duration) -> anyhow::Result<serde_json::Value> {
        let deadline = Instant::now() + budget;
        let mut line = String::new();
        loop {
            line.clear();
            let read = self.stdout.read_line(&mut line)?;
            anyhow::ensure!(read > 0, "the sidecar closed its stdout before answering");
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                return Ok(value);
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "the sidecar never produced a JSON frame"
            );
        }
    }

    /// Sends one `_shell/` request and waits for the response with its id.
    fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
        budget: Duration,
    ) -> anyhow::Result<serde_json::Value> {
        let id = format!(
            "test-{}-{}",
            method.rsplit('/').next().unwrap_or("call"),
            uuid_like()
        );
        self.write(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "secret": self.secret,
            "sidecar_epoch": self.epoch,
            "params": params,
        }))?;
        let deadline = Instant::now() + budget;
        loop {
            let frame = self.read_frame(Duration::from_secs(5))?;
            // Business frames and outbound management traffic share this stdout.
            if frame.get("id").and_then(|v| v.as_str()) != Some(id.as_str()) {
                anyhow::ensure!(
                    Instant::now() < deadline,
                    "no answer to {method}: last frame was {frame}"
                );
                continue;
            }
            return Ok(frame);
        }
    }
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Reads frames until the one carrying `id`, so a caller can send several requests
/// first and only then wait.
fn read_until_id(
    sidecar: &mut Sidecar,
    id: &str,
    budget: Duration,
) -> anyhow::Result<serde_json::Value> {
    let deadline = Instant::now() + budget;
    loop {
        let frame = sidecar.read_frame(Duration::from_secs(5))?;
        if frame.get("id").and_then(|v| v.as_str()) == Some(id) {
            return Ok(frame);
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "no answer to {id}: last frame was {frame}"
        );
    }
}

/// A v4-shaped random string without pulling in a uuid dependency for the test.
fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}{:x}", std::process::id())
}

// ---------------------------------------------------------------------------
// A01: the shipped entry points reach the managed lifecycle with no env selector.
// ---------------------------------------------------------------------------

/// A demand-driven prepare over the private channel brings up one guardian and one
/// business daemon, and reports the identity it actually found.
///
/// This is the desktop's whole startup contract minus the GUI: a shell says "make
/// this project ready", and gets either a runtime that matches the artifact it is
/// running or a truthful refusal. Nothing here sets `YI_AGENT_BRINGUP`; the channel
/// itself is the explicit selection, which is what the design asks for.
#[test]
fn a_prepare_over_the_private_channel_brings_up_one_managed_runtime() -> anyhow::Result<()> {
    let mut fixture = IsolatedRuntimeFixture::start_without_bringup_env()?;
    let mut sidecar = Sidecar::spawn(&mut fixture)?;

    let params = serde_json::to_value(yi_agent_lifecycle::production::PrepareRequest {
        scope: fixture.scope(),
        expected: fixture.artifact().clone(),
        attempt_id: format!("attempt-{}", uuid_like()),
    })?;
    let answer = sidecar.call("_shell/prepare", params, START_BUDGET)?;

    anyhow::ensure!(answer.get("error").is_none(), "prepare refused: {answer}");
    let result = answer
        .get("result")
        .ok_or_else(|| anyhow::anyhow!("prepare answered without a result: {answer}"))?;
    // `Connected` and `VersionChoiceRequired` both carry an identity; which one it is
    // depends only on whether the runtime this fixture started is the same build.
    let identity = result
        .get("Connected")
        .or_else(|| result.get("VersionChoiceRequired"))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "prepare did not connect to a runtime of this build: {result}; \
                 this is the failure that leaves a user on the embedded path"
            )
        })?;
    assert_eq!(
        identity.get("mode").and_then(|v| v.as_str()),
        Some("managed"),
        "a prepared runtime must report managed mode: {identity}"
    );

    // One guardian, one daemon: the guardian's journal is the runtime's own record
    // of what it started, so counting it is not counting the harness.
    let journal = journal_records(&fixture.runtime_dir);
    assert!(
        !journal.is_empty(),
        "the runtime recorded no processes; a connected prepare must have started one"
    );
    assert!(
        fixture.runtime_dir.join("runtime.sqlite").exists(),
        "the business daemon must have created its database"
    );
    Ok(())
}

/// The same channel refuses a `_shell/` call that did not bootstrap first.
///
/// Fail-closed matters here more than anywhere else in this file: `prepare` starts a
/// runtime, so an unauthenticated caller reaching it would let any local process
/// start work under the user's instance.
#[test]
fn an_unbootstrapped_management_call_starts_nothing() -> anyhow::Result<()> {
    let mut fixture = IsolatedRuntimeFixture::start_without_bringup_env()?;
    let mut command = Command::new(common::yi_agent_bin());
    fixture.apply_env(&mut command);
    command
        .args(["app-server", "--listen", "stdio://"])
        .env("YI_AGENT_LOCAL_MANAGEMENT", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn()?;
    fixture.own(&child, "unbootstrapped-sidecar")?;
    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));

    let id = format!("nope-{}", uuid_like());
    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "_shell/prepare",
            "secret": "not-the-secret",
            "sidecar_epoch": "not-the-epoch",
            "params": serde_json::to_value(yi_agent_lifecycle::production::PrepareRequest {
                scope: fixture.scope(),
                expected: fixture.artifact().clone(),
                attempt_id: format!("attempt-{}", uuid_like()),
            })?,
        })
    )?;
    stdin.flush()?;

    let mut line = String::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    let answer = loop {
        line.clear();
        let read = stdout.read_line(&mut line)?;
        anyhow::ensure!(read > 0, "the sidecar closed before refusing");
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim())
            && value.get("id").and_then(|v| v.as_str()) == Some(id.as_str())
        {
            break value;
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "the sidecar never answered the unauthenticated prepare"
        );
    };
    assert!(
        answer.get("error").is_some(),
        "an unbootstrapped prepare must be refused, not answered: {answer}"
    );

    // The refusal has to be about the *side effect*, not the message: a runtime that
    // started anyway would be running work nobody asked for.
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !fixture.runtime_dir.join("runtime.sqlite").exists(),
        "a refused prepare started a runtime anyway"
    );
    let _ = child.kill();
    Ok(())
}

/// The runtime a frontend asks for must match its own artifact, or the prepare says
/// so instead of adopting it.
///
/// This is the negative that keeps the positive honest: an implementation that
/// answered `Connected` unconditionally would pass the test above and fail this one.
#[test]
fn a_prepare_for_another_build_is_a_choice_not_a_silent_adoption() -> anyhow::Result<()> {
    let mut fixture = IsolatedRuntimeFixture::start_without_bringup_env()?;
    let mut sidecar = Sidecar::spawn(&mut fixture)?;

    let mut other = fixture.artifact().clone();
    other.build_id = "some-other-build".to_string();
    other.sha256 = "ff".repeat(32);
    let params = serde_json::to_value(yi_agent_lifecycle::production::PrepareRequest {
        scope: fixture.scope(),
        expected: other,
        attempt_id: format!("attempt-{}", uuid_like()),
    })?;
    let answer = sidecar.call("_shell/prepare", params, START_BUDGET)?;
    let rendered = answer.to_string();
    assert!(
        rendered.contains("VersionChoiceRequired") || rendered.contains("Blocked"),
        "a different build must not be adopted silently: {answer}"
    );
    assert!(
        !rendered.contains("\"Connected\""),
        "prepare claimed the wrong build was connected: {answer}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Real TUI and headless entry points.
// ---------------------------------------------------------------------------

/// The default TUI entry, on a real pty.
///
/// A pty is not optional: crossterm refuses to enter raw mode without a controlling
/// terminal, and the TUI's whole startup -- including the lease it registers -- lives
/// behind that. Running it on pipes would test a code path a user never takes.
struct TuiTerminal {
    child: Child,
    master: std::os::unix::io::RawFd,
}

impl TuiTerminal {
    fn spawn(fixture: &mut IsolatedRuntimeFixture, port: u16) -> anyhow::Result<Self> {
        let mut master = 0i32;
        let mut slave = 0i32;
        let mut size = libc::winsize {
            ws_row: 40,
            ws_col: 120,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: both descriptors are written by the call before they are read, and
        // the termios/winsize pointers are the null/sized values `openpty` documents.
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size,
            )
        };
        anyhow::ensure!(
            rc == 0,
            "openpty failed: {}",
            std::io::Error::last_os_error()
        );
        use std::os::fd::FromRawFd;
        // SAFETY: `openpty` returned both descriptors and hands ownership over.
        let slave_for_child = unsafe { std::os::fd::OwnedFd::from_raw_fd(slave) };
        let master_for_parent = unsafe { std::os::fd::OwnedFd::from_raw_fd(master) };

        let mut command = Command::new(common::yi_agent_bin());
        fixture.apply_env(&mut command);
        command
            .arg("--workdir")
            .arg(&fixture.workdir)
            .args(["--provider", "openai"])
            .arg("--api-url")
            .arg(format!("http://127.0.0.1:{port}"))
            .args(["--api-key", "test-key", "--model", "gpt-4o", "--yolo"])
            .env("TERM", "xterm-256color")
            .stdin(Stdio::from(slave_for_child))
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let child = command.spawn()?;
        fixture.own(&child, "tui")?;
        use std::os::fd::IntoRawFd;
        Ok(Self {
            child,
            master: master_for_parent.into_raw_fd(),
        })
    }

    /// Waits until the TUI has attached a root to the runtime.
    ///
    /// Root attach is the last step before the lease is registered in the same driver
    /// iteration, so a served socket is the closest honest readiness signal the
    /// protocol offers.
    fn await_runtime_attached(&mut self, fixture: &IsolatedRuntimeFixture) -> anyhow::Result<()> {
        let deadline = Instant::now() + START_BUDGET;
        loop {
            if service_answers(&fixture.runtime_dir) {
                std::thread::sleep(Duration::from_millis(500));
                return Ok(());
            }
            if let Ok(Some(status)) = self.child.try_wait() {
                anyhow::bail!("the TUI exited before attaching a root: {status}");
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "the TUI never attached a root under {}; stderr: {}",
                fixture.runtime_dir.display(),
                std::fs::read_to_string(fixture.root.path().join("tui-stderr.log"))
                    .unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Ctrl+Q, the TUI's unconditional quit.
    fn quit(&mut self) -> anyhow::Result<()> {
        let bytes = [0x11u8];
        // SAFETY: `master` is an open descriptor owned by this value and the buffer
        // is alive for the call.
        let written = unsafe {
            libc::write(
                self.master,
                bytes.as_ptr().cast::<libc::c_void>(),
                bytes.len(),
            )
        };
        anyhow::ensure!(written > 0, "could not type into the TUI's terminal");
        Ok(())
    }

    fn wait_exit(&mut self, budget: Duration) -> anyhow::Result<()> {
        let deadline = Instant::now() + budget;
        loop {
            if let Ok(Some(_)) = self.child.try_wait() {
                return Ok(());
            }
            anyhow::ensure!(Instant::now() < deadline, "the TUI did not exit");
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for TuiTerminal {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // SAFETY: the descriptor is owned by this value and closed exactly once.
        unsafe { libc::close(self.master) };
    }
}

/// The real `run` entry point, in its own process.
struct HeadlessRun {
    child: Child,
    stderr: Option<std::process::ChildStderr>,
    /// Captured at spawn so a later assertion can ask about *this* process rather
    /// than about whatever holds its pid.
    identity: StartIdentity,
}

impl HeadlessRun {
    fn spawn(
        fixture: &mut IsolatedRuntimeFixture,
        port: u16,
        prompt: &str,
    ) -> anyhow::Result<Self> {
        let mut command = Command::new(common::yi_agent_bin());
        fixture.apply_env(&mut command);
        command
            .arg("--workdir")
            .arg(&fixture.workdir)
            .args(["--provider", "openai"])
            .arg("--api-url")
            .arg(format!("http://127.0.0.1:{port}"))
            .args(["--api-key", "test-key", "--model", "gpt-4o", "--yolo"])
            .args(["run", "--subagents", prompt])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn()?;
        fixture.own(&child, "headless")?;
        let stderr = child.stderr.take();
        let identity = start_identity(child.id() as i32)
            .ok_or_else(|| anyhow::anyhow!("the headless run exited before it was identified"))?;
        Ok(Self {
            child,
            stderr,
            identity,
        })
    }

    fn wait_exit(&mut self, budget: Duration) -> anyhow::Result<std::process::ExitStatus> {
        let deadline = Instant::now() + budget;
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                // A timed-out run is a failure, never a clean stop. Attach whatever
                // the child said so the report points at the real cause instead of
                // "it did not exit".
                anyhow::bail!(
                    "the headless run did not exit within {budget:?}; stderr: {}",
                    self.take_stderr()
                );
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// Reads what the child wrote to stderr, so a failed assertion can quote it.
    ///
    /// Only called on a path that already decided to fail, so draining here cannot
    /// hide output a passing test would have wanted.
    fn take_stderr(&mut self) -> String {
        let Some(mut stderr) = self.stderr.take() else {
            return "<already drained>".to_string();
        };
        let mut text = String::new();
        let _ = std::io::Read::read_to_string(&mut stderr, &mut text);
        text
    }
}

impl Drop for HeadlessRun {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------
// A16: the headless entry point is an explicit managed frontend, not a fallback.
// ---------------------------------------------------------------------------

/// The shipped `run` entry walks the whole managed start and then releases.
///
/// This is the A16 half that is about *the entry point*: the design says the
/// supported frontends select `for_managed_frontend` explicitly, and keeps
/// `default_for_build` embedded so a manual `daemon` keeps owning its runtime. The
/// trace is the observable difference -- a run that fell back to an embedded daemon
/// would produce no stage at all, and its work would be unreclaimable.
#[test]
fn the_headless_entry_point_is_a_managed_frontend() -> anyhow::Result<()> {
    let mut fixture = IsolatedRuntimeFixture::start_without_bringup_env()?;
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let _gateway = spawn_mock_gateway(listener);
    let trace_path = fixture.root.path().join("startup-trace.log");

    let mut run = {
        let mut command = Command::new(common::yi_agent_bin());
        fixture.apply_env(&mut command);
        command
            .arg("--workdir")
            .arg(&fixture.workdir)
            .args(["--provider", "openai"])
            .arg("--api-url")
            .arg(format!("http://127.0.0.1:{port}"))
            .args(["--api-key", "test-key", "--model", "gpt-4o", "--yolo"])
            .args(["run", "--subagents", "say done"])
            .env("YI_AGENT_STARTUP_TRACE", &trace_path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn()?;
        fixture.own(&child, "headless-explicit")?;
        let stderr = child.stderr.take();
        let identity = start_identity(child.id() as i32)
            .ok_or_else(|| anyhow::anyhow!("the headless run exited before it was identified"))?;
        HeadlessRun {
            child,
            stderr,
            identity,
        }
    };

    let status = run.wait_exit(START_BUDGET)?;
    let trace = startup_trace(&fixture);
    let stages = trace
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    assert!(
        status.success(),
        "the managed run must finish cleanly; trace: {trace}"
    );
    assert_eq!(
        stages,
        MANAGED_STAGES.to_vec(),
        "the shipped run entry must walk every managed stage in order; trace: {trace}"
    );

    // A run that finished cleanly released its lease; a reclaim that only appears
    // after 30 s would be expiry, not release.
    let record =
        wait_for_reclaim(&fixture.runtime_dir, "run:", RELEASE_BUDGET).ok_or_else(|| {
            anyhow::anyhow!(
                "the run never became a client of the runtime, or never released: {:?}",
                reclaim_records(&fixture.runtime_dir)
                    .into_iter()
                    .map(|record| record.result.client_run_id)
                    .collect::<Vec<_>>()
            )
        })?;
    assert!(
        record.result.stopped,
        "a released lease must prove its work stopped: {:?}",
        record.result.failures
    );
    Ok(())
}

/// The shipped TUI entry is a managed frontend too, and quitting releases its lease.
///
/// Same claim as the headless test, on the entry point a user actually opens. The pty
/// is load-bearing: without a controlling terminal the TUI never reaches the lease,
/// and "it quit" would be the only thing proven.
#[test]
fn the_tui_entry_point_is_a_managed_frontend() -> anyhow::Result<()> {
    let mut fixture = IsolatedRuntimeFixture::start_without_bringup_env()?;
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let _gateway = spawn_mock_gateway(listener);
    let trace_path = fixture.root.path().join("startup-trace.log");

    let mut terminal = {
        // The TUI reads the trace path from its own environment, so it has to be set
        // on the child rather than in this process.
        let mut master = 0i32;
        let mut slave = 0i32;
        let mut size = libc::winsize {
            ws_row: 40,
            ws_col: 120,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: as in `TuiTerminal::spawn`.
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size,
            )
        };
        anyhow::ensure!(
            rc == 0,
            "openpty failed: {}",
            std::io::Error::last_os_error()
        );
        use std::os::fd::FromRawFd;
        let slave_for_child = unsafe { std::os::fd::OwnedFd::from_raw_fd(slave) };
        let master_for_parent = unsafe { std::os::fd::OwnedFd::from_raw_fd(master) };
        let mut command = Command::new(common::yi_agent_bin());
        fixture.apply_env(&mut command);
        command
            .arg("--workdir")
            .arg(&fixture.workdir)
            .args(["--provider", "openai"])
            .arg("--api-url")
            .arg(format!("http://127.0.0.1:{port}"))
            .args(["--api-key", "test-key", "--model", "gpt-4o", "--yolo"])
            .env("TERM", "xterm-256color")
            .env("YI_AGENT_STARTUP_TRACE", &trace_path)
            .stdin(Stdio::from(slave_for_child))
            .stdout(Stdio::null())
            .stderr(Stdio::from(std::fs::File::create(
                fixture.root.path().join("tui-stderr.log"),
            )?));
        let child = command.spawn()?;
        fixture.own(&child, "tui-managed")?;
        use std::os::fd::IntoRawFd;
        TuiTerminal {
            child,
            master: master_for_parent.into_raw_fd(),
        }
    };

    terminal.await_runtime_attached(&fixture)?;

    let trace = startup_trace(&fixture);
    let stages = trace
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    assert_eq!(
        stages,
        MANAGED_STAGES.to_vec(),
        "the shipped TUI entry must walk every managed stage in order; trace: {trace}; screen: {:?}",
        screen_text(&terminal)
    );

    terminal.quit()?;
    terminal.wait_exit(RELEASE_BUDGET)?;
    let record =
        wait_for_reclaim(&fixture.runtime_dir, "tui:", RELEASE_BUDGET).ok_or_else(|| {
            anyhow::anyhow!("the TUI never became a client of the runtime, or never released")
        })?;
    assert!(
        record.result.stopped,
        "the TUI's exit must reclaim its work: {:?}",
        record.result.failures
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// A04/A06: two real frontends, one runtime, and quitting means one client's work.
// ---------------------------------------------------------------------------

/// A `bash` command that writes a marker, then blocks until the test removes it.
///
/// The blocking is what makes "this work was reclaimed" observable: a command that
/// already finished would leave nothing for the runtime to stop, and a marker file
/// alone proves only that it once ran.
fn marker_work_command(marker: &Path) -> String {
    format!(
        "printf %s $$ > {marker} && while [ -f {marker} ]; do sleep 0.2; done",
        marker = marker.display()
    )
}

/// Waits until the marker exists, then returns the pid the shell wrote into it.
fn await_marker_pid(marker: &Path, budget: Duration) -> anyhow::Result<StartIdentity> {
    let deadline = Instant::now() + budget;
    loop {
        if let Ok(text) = std::fs::read_to_string(marker)
            && let Ok(pid) = text.trim().parse::<i32>()
            && let Some(identity) = start_identity(pid)
        {
            return Ok(identity);
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "the marker process never started: {}",
            marker.display()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// One OpenAI-compatible endpoint that asks for exactly one `bash` call.
fn spawn_tool_calling_gateway(
    listener: std::net::TcpListener,
    command: String,
) -> std::thread::JoinHandle<()> {
    use std::io::Read as _;
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut buffer = vec![0u8; 65536];
            let read = stream.read(&mut buffer).unwrap_or(0);
            let body = String::from_utf8_lossy(&buffer[..read]).to_string();
            if body.lines().next().unwrap_or("").contains("/responses") {
                let _ = stream.write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                continue;
            }
            let saw_tool_result =
                body.contains("\"role\":\"tool\"") || body.contains("\"role\": \"tool\"");
            let frames = if saw_tool_result {
                vec![
                    r#"{"choices":[{"delta":{"content":"done"}}]}"#.to_string(),
                    r#"{"choices":[{"finish_reason":"stop","delta":{}}]}"#.to_string(),
                ]
            } else {
                let args = serde_json::json!({ "command": command, "timeout": 120 }).to_string();
                vec![
                    r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"bash","arguments":""}}]}}]}"#.to_string(),
                    format!(
                        r#"{{"choices":[{{"delta":{{"tool_calls":[{{"index":0,"function":{{"arguments":{}}}}}]}}}}]}}"#,
                        serde_json::to_string(&args).unwrap()
                    ),
                    r#"{"choices":[{"finish_reason":"tool_calls","delta":{}}]}"#.to_string(),
                ]
            };
            let mut out = String::from(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            );
            for frame in frames {
                out.push_str(&format!("data: {frame}\n\n"));
            }
            out.push_str("data: [DONE]\n\n");
            let _ = stream.write_all(out.as_bytes());
            let _ = stream.flush();
        }
    })
}

/// A TUI and a headless run share one runtime; quitting the TUI leaves the run's
/// work alone.
///
/// A06 is the load-bearing half. "Two leases exist" is easy; the claim is that the
/// reclaim which follows one client's exit stops *that client's* owned processes and
/// nobody else's. So the TUI starts a blocking marker process, the run starts its
/// own, and after the TUI quits the marker it started must be gone while the other
/// must still be the same process.
#[test]
fn quitting_one_frontend_leaves_the_others_work_running() -> anyhow::Result<()> {
    let mut fixture = IsolatedRuntimeFixture::start_without_bringup_env()?;
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let tui_marker = fixture.root.path().join("tui-work.pid");
    let _gateway = spawn_tool_calling_gateway(listener, marker_work_command(&tui_marker));

    let mut terminal = TuiTerminal::spawn(&mut fixture, port)?;
    terminal.await_runtime_attached(&fixture)?;

    // The TUI must actually own a process before "its work survived/stopped" means
    // anything. Typing at it is the honest way to make it start one.
    type_prompt(&mut terminal, "run the command")?;
    let tui_work = await_marker_pid(&tui_marker, Duration::from_secs(60))?;

    // A second frontend on the same runtime: the run gets its own lease.
    let run_marker = fixture.root.path().join("run-work.pid");
    // Its own gateway, because the tool command differs and the mock answers per
    // process.
    let run_listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let run_port = run_listener.local_addr()?.port();
    let _run_gateway = spawn_tool_calling_gateway(run_listener, marker_work_command(&run_marker));
    let mut run = HeadlessRun::spawn(&mut fixture, run_port, "run the command")?;
    let run_work = await_marker_pid(&run_marker, Duration::from_secs(60))?;

    // Let the runtime record both clients before either leaves: a reclaim can only
    // leave alone work it knows is somebody else's.
    //
    // Waiting for the *fact* rather than a fixed sleep: under load the guardian can
    // take far longer than any constant to journal both owners, and a sleep long
    // enough to hide that would make the suite slow for every other case.
    await_two_owners_in_journal(&fixture.runtime_dir, Duration::from_secs(60)).map_err(
        |error| {
            anyhow::anyhow!(
                "{error}; journal: {:?}",
                journal_records(&fixture.runtime_dir)
            )
        },
    )?;

    terminal.quit()?;
    terminal.wait_exit(RELEASE_BUDGET)?;
    let record = wait_for_reclaim(&fixture.runtime_dir, "tui:", RELEASE_BUDGET)
        .ok_or_else(|| anyhow::anyhow!("the TUI's exit produced no reclaim record"))?;
    assert!(
        record.result.stopped,
        "the TUI's exit must stop its own work: {:?}",
        record.result.failures
    );

    // The claim: the TUI's process is gone, the run's is untouched.
    let deadline = Instant::now() + RELEASE_BUDGET;
    while start_identity(tui_work.pid).as_ref() == Some(&tui_work) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        start_identity(tui_work.pid).as_ref() != Some(&tui_work),
        "the TUI's owned process {} survived its own reclaim",
        tui_work.pid
    );

    // Before blaming the TUI for the run's work, rule out the two ways the run could
    // have stopped it itself: its process exiting, or its own frontend releasing.
    // Conflating those with a theft would make this test accuse the runtime of a bug
    // whenever the *other* frontend simply finished or failed.
    let run_alive = fixture.same_process_alive(&run.identity);
    let run_reclaim = reclaim_records(&fixture.runtime_dir)
        .into_iter()
        .find(|record| record.result.client_run_id.starts_with("run:"));
    assert_eq!(
        start_identity(run_work.pid).as_ref(),
        Some(&run_work),
        "quitting the TUI reclaimed the headless run's work: pid {}; \
         run process alive={run_alive}; run's own reclaim={run_reclaim:?}; \
         journal={:?}",
        run_work.pid,
        journal_records(&fixture.runtime_dir)
    );

    // And this fixture cleans up its own run rather than leaving it to the runtime.
    let _ = run.child.kill();
    let _ = run.child.wait();
    let _ = std::fs::remove_file(&run_marker);
    Ok(())
}

/// Types a prompt into the TUI and sends Enter.
fn type_prompt(terminal: &mut TuiTerminal, text: &str) -> anyhow::Result<()> {
    let mut bytes = text.as_bytes().to_vec();
    bytes.push(b'\r');
    // SAFETY: `master` is an open descriptor owned by the terminal value, and the
    // buffer is alive for the call.
    let written = unsafe {
        libc::write(
            terminal.master,
            bytes.as_ptr().cast::<libc::c_void>(),
            bytes.len(),
        )
    };
    anyhow::ensure!(written > 0, "could not type into the TUI's terminal");
    Ok(())
}

/// Reads the TUI's screen so a failure reports what the user would have seen.
fn screen_text(terminal: &TuiTerminal) -> String {
    // The master side sees every byte the child writes, but reads would block once
    // the buffer is drained. `FIONREAD` says whether anything is queued *now*: zero
    // means "no output yet", not "no output".
    let mut available: libc::c_int = 0;
    // SAFETY: `master` is an open descriptor owned by the terminal value, and the
    // out-parameter points at a live `c_int`.
    let ready = unsafe { libc::ioctl(terminal.master, libc::FIONREAD, &mut available) };
    if ready != 0 || available <= 0 {
        return String::new();
    }
    let mut buffer = vec![0u8; available as usize];
    // SAFETY: the buffer is `available` bytes long, matching the length passed.
    let read = unsafe {
        libc::read(
            terminal.master,
            buffer.as_mut_ptr().cast::<libc::c_void>(),
            buffer.len(),
        )
    };
    if read <= 0 {
        return String::new();
    }
    buffer.truncate(read as usize);
    String::from_utf8_lossy(&buffer).into_owned()
}

// ---------------------------------------------------------------------------
// A07: the last client leaving stops the runtime, with real stop evidence.
// ---------------------------------------------------------------------------

/// After the only frontend quits, the runtime really stops.
///
/// The three observations are deliberately not interchangeable:
///
/// * the process tree is gone -- compared by start identity, so a recycled pid
///   cannot make a dead process look alive;
/// * the instance lock is free -- an unlinked socket proves nothing about the lock,
///   and the lock is what actually prevents two daemons;
/// * the service does not answer -- a socket *file* can outlive the listener.
#[test]
fn the_last_frontend_leaving_stops_the_runtime_for_real() -> anyhow::Result<()> {
    let mut fixture = IsolatedRuntimeFixture::start_without_bringup_env()?;
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let _gateway = spawn_mock_gateway(listener);

    let mut run = HeadlessRun::spawn(&mut fixture, port, "say done")?;
    let status = run.wait_exit(START_BUDGET)?;
    assert!(status.success(), "the managed run must finish cleanly");

    // The run's own release is the trigger; a record proves it happened rather than
    // the lease merely expiring.
    let record = wait_for_reclaim(&fixture.runtime_dir, "run:", RELEASE_BUDGET)
        .ok_or_else(|| anyhow::anyhow!("the run never registered a lease, or never released it"))?;
    assert!(
        record.result.stopped,
        "the run's release did not stop its work: {:?}",
        record.result.failures
    );

    // The guardian journal records every process it started; each of those is what
    // this test now insists is gone. No stale entry is treated as "already dead":
    // the comparison is against the start identity.
    let journal = journal_records(&fixture.runtime_dir);
    let mut recorded = Vec::new();
    for entry in &journal {
        if let Some(pid) = entry.get("pid").and_then(|value| value.as_i64())
            && let Some(identity) = start_identity(pid as i32)
        {
            recorded.push(format!("{}({pid})", identity.started));
        }
    }
    fixture.assert_runtime_stopped(RELEASE_BUDGET)?;
    assert!(
        recorded.is_empty()
            || fixture
                .owned
                .iter()
                .all(|owned| !fixture.same_process_alive(&owned.identity)),
        "the journal listed processes still alive: {recorded:?}"
    );
    Ok(())
}

/// A configuration-only resident registration does not keep a retired instance's
/// daemon alive, and does not start one across watch ticks.
///
/// A12/A07 overlap here by design: the registration is *configuration* ("this
/// project wants a daemon when a frontend is around"), not an instruction to keep one
/// with no frontend. Once the instance is retired the unattended loop must stand
/// down, and "no daemon" has to survive more than one tick -- a loop that restarted
/// one would be indistinguishable from a single tick that happened to miss.
#[test]
fn a_retired_instance_does_not_resurrect_a_daemon_across_watch_ticks() -> anyhow::Result<()> {
    let fixture = IsolatedRuntimeFixture::start_without_bringup_env()?;
    let instance_home = fixture.instance_home.clone();
    let resident_dir = instance_home.clone();

    // A registration is what a plugin install writes: the project wants a daemon.
    yi_agent_store::resident::require(&resident_dir, &fixture.workdir, "fixture-plugin")?;
    // Retire the instance the way a completed migration does.
    yi_agent_supervisors::lifecycle::retirement::retire(
        &instance_home,
        "migrated to the managed runtime",
    )?;

    let mut starts = 0usize;
    for _ in 0..3 {
        let outcome = yi_agent_supervisors::watch::once_unattended(
            &instance_home,
            &resident_dir,
            &mut |projects| starts += projects.len(),
        );
        assert_eq!(
            outcome, None,
            "a retired instance must stand down, not report an idle tick"
        );
    }
    assert_eq!(
        starts, 0,
        "a retired instance started work for a configuration-only registration"
    );
    assert!(
        !fixture.runtime_dir.join("runtime.sqlite").exists(),
        "no daemon may start unattended after retirement"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// A02: two windows, one project, one runtime -- including through a symlink.
// ---------------------------------------------------------------------------

/// Two sidecars preparing the same project at once still end up on one runtime.
///
/// The failure this guards is not a crash but a duplicate: two guardians, or two
/// daemons serving one database, each believing it is the project's runtime. So the
/// assertion is on *counts* read from the runtime's own journal, not on "both said
/// Connected" -- an implementation that started two runtimes and reported the
/// second one would pass the weaker check.
#[test]
fn two_concurrent_prepares_yield_one_runtime() -> anyhow::Result<()> {
    let mut fixture = IsolatedRuntimeFixture::start_without_bringup_env()?;
    let mut first = Sidecar::spawn(&mut fixture)?;
    let mut second = Sidecar::spawn(&mut fixture)?;

    let params = || -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::to_value(
            yi_agent_lifecycle::production::PrepareRequest {
                scope: fixture.scope(),
                expected: fixture.artifact().clone(),
                attempt_id: format!("attempt-{}", uuid_like()),
            },
        )?)
    };

    // Both requests are in flight before either answer is read: issuing them one at
    // a time would only prove the second observes a *finished* start, which is the
    // easy half.
    first.write(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": "race-a",
        "method": "_shell/prepare",
        "secret": first.secret.clone(),
        "sidecar_epoch": first.epoch.clone(),
        "params": params()?,
    }))?;
    second.write(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": "race-b",
        "method": "_shell/prepare",
        "secret": second.secret.clone(),
        "sidecar_epoch": second.epoch.clone(),
        "params": params()?,
    }))?;

    let answer_a = read_until_id(&mut first, "race-a", START_BUDGET)?;
    let answer_b = read_until_id(&mut second, "race-b", START_BUDGET)?;
    for (label, answer) in [("a", &answer_a), ("b", &answer_b)] {
        anyhow::ensure!(
            answer.get("error").is_none(),
            "sidecar {label} refused the concurrent prepare: {answer}"
        );
        let rendered = answer.to_string();
        assert!(
            rendered.contains("\"Connected\"") || rendered.contains("VersionChoiceRequired"),
            "sidecar {label} did not reach a runtime: {answer}"
        );
    }

    // The journal is the runtime's own record, so one daemon entry means one daemon
    // and two would mean the race produced a duplicate.
    let daemons = journal_records(&fixture.runtime_dir)
        .iter()
        .filter(|record| record.get("owner").and_then(|o| o.get("daemon")).is_some())
        .count();
    assert_eq!(
        daemons, 1,
        "two concurrent prepares started {daemons} daemons; one project owns one runtime"
    );
    Ok(())
}

/// A project reached through a symlink is the same scope, not a second runtime.
///
/// This is the multi-window case in its most common disguise: one window opened the
/// real path and another opened a symlink to it. If the scope were the lexical path,
/// the two windows would get two runtimes over one database -- and the damage would
/// only show up later, as a locked or corrupted store.
#[test]
fn a_symlinked_project_path_resolves_to_the_same_scope() -> anyhow::Result<()> {
    let fixture = IsolatedRuntimeFixture::start_without_bringup_env()?;
    let link = fixture.root.path().join("repository-link");
    std::os::unix::fs::symlink(&fixture.workdir, &link)?;

    // The resolution the guardian does, applied to both spellings of one project.
    let real = yi_agent_lifecycle::scope::resolve(
        &fixture.runtime_dir,
        &fixture.workdir,
        &fixture.instance_home,
    )?;
    let via_link =
        yi_agent_lifecycle::scope::resolve(&fixture.runtime_dir, &link, &fixture.instance_home)?;

    assert_eq!(
        real.project_key, via_link.project_key,
        "a symlinked project path resolved to a different project; two windows would \
         each start their own runtime for one repository"
    );
    assert_eq!(
        real.key, via_link.key,
        "a symlinked project path resolved to a different runtime directory"
    );
    // Not a tautology: the link text itself differs, so a lexical comparison would
    // have produced two keys.
    assert_ne!(
        link.to_string_lossy(),
        fixture.workdir.to_string_lossy(),
        "the fixture did not actually create a divergent path"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// A18: a stale or forged management frame reaches nothing.
// ---------------------------------------------------------------------------

/// After a real bootstrap, a frame with another epoch or another secret is refused.
///
/// The two rejects are different attacks and both matter:
///
/// * a *wrong epoch* is a stale frame from a previous sidecar generation -- the
///   design's rule is that a frame cannot nominate which generation its answer
///   belongs to;
/// * a *wrong secret* is a local process that guessed, or read an old log. The
///   private channel is the only thing separating the shell from every other local
///   process, so this rejection is load-bearing rather than cosmetic.
#[test]
fn a_stale_epoch_or_foreign_secret_is_refused_after_bootstrap() -> anyhow::Result<()> {
    let mut fixture = IsolatedRuntimeFixture::start_without_bringup_env()?;
    let mut sidecar = Sidecar::spawn(&mut fixture)?;

    let prepare_params = || -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::to_value(
            yi_agent_lifecycle::production::PrepareRequest {
                scope: fixture.scope(),
                expected: fixture.artifact().clone(),
                attempt_id: format!("attempt-{}", uuid_like()),
            },
        )?)
    };

    for (label, secret, epoch) in [
        (
            "stale epoch",
            sidecar.secret.clone(),
            "epoch-from-a-previous-sidecar".to_string(),
        ),
        (
            "foreign secret",
            "not-the-secret".to_string(),
            sidecar.epoch.clone(),
        ),
    ] {
        let id = format!("forged-{}", uuid_like());
        sidecar.write(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "_shell/prepare",
            "secret": secret,
            "sidecar_epoch": epoch,
            "params": prepare_params()?,
        }))?;
        let answer = read_until_id(&mut sidecar, &id, START_BUDGET)?;
        assert!(
            answer.get("error").is_some(),
            "a frame with a {label} was admitted: {answer}"
        );
        assert!(
            !answer.to_string().contains("\"Connected\""),
            "a frame with a {label} reached a runtime: {answer}"
        );
    }

    // A rejected frame must not have started anything: a refusal that still ran the
    // side effect would be the worst of both.
    assert!(
        !fixture.runtime_dir.join("runtime.sqlite").exists(),
        "a forged management frame started a runtime"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// A04: the sidecar that launched the runtime is not its lifeline.
// ---------------------------------------------------------------------------

/// Killing the sidecar that originally started the runtime does not disturb a
/// frontend that is still attached and working.
///
/// Design §4.4 makes this a process-level requirement, not an API promise: the
/// launcher's descriptor is a bootstrap proof, so the runtime must not treat "whoever
/// started me is gone" as "stop". A TUI holding a lease and running a real process is
/// the witness; the three observations are that its work is untouched, the control
/// channel still answers, and the guardian is the same process it was.
#[test]
fn a_dead_launcher_sidecar_does_not_stop_a_working_runtime() -> anyhow::Result<()> {
    let mut fixture = IsolatedRuntimeFixture::start_without_bringup_env()?;

    // The sidecar brings the runtime up, exactly as the desktop's first prepare does.
    let mut sidecar = Sidecar::spawn(&mut fixture)?;
    let params = serde_json::to_value(yi_agent_lifecycle::production::PrepareRequest {
        scope: fixture.scope(),
        expected: fixture.artifact().clone(),
        attempt_id: format!("attempt-{}", uuid_like()),
    })?;
    let answer = sidecar.call("_shell/prepare", params, START_BUDGET)?;
    anyhow::ensure!(
        answer.get("error").is_none() && answer.to_string().contains("\"Connected\""),
        "the runtime never came up: {answer}"
    );
    let guardian_before = journal_generation(&fixture.runtime_dir);
    anyhow::ensure!(
        guardian_before.is_some(),
        "the runtime recorded no guardian generation"
    );

    // A frontend that is attached and doing work.
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let marker = fixture.root.path().join("survivor-work.pid");
    let _gateway = spawn_tool_calling_gateway(listener, marker_work_command(&marker));
    let mut terminal = TuiTerminal::spawn(&mut fixture, port)?;
    terminal.await_runtime_attached(&fixture)?;
    type_prompt(&mut terminal, "run the command")?;
    let work = await_marker_pid(&marker, Duration::from_secs(60))?;

    // Now the launcher dies. `SIGKILL`, because a graceful exit could let an
    // implementation pass by cleaning up while the abrupt case -- the one a crash
    // produces -- still failed.
    let sidecar_pid = sidecar.child.id() as i32;
    let sidecar_identity = start_identity(sidecar_pid);
    // SAFETY: the pid names a child this fixture owns; killing it is the test's whole
    // intent.
    unsafe { libc::kill(sidecar_pid, libc::SIGKILL) };
    let _ = sidecar.child.wait();
    let deadline = Instant::now() + Duration::from_secs(10);
    while start_identity(sidecar_pid) == sidecar_identity && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }

    // Give a supervisor that wrongly ties the runtime to its launcher time to act.
    std::thread::sleep(Duration::from_secs(2));

    assert_eq!(
        start_identity(work.pid).as_ref(),
        Some(&work),
        "the launcher's death stopped work a still-attached frontend was running"
    );
    assert!(
        service_answers(&fixture.runtime_dir),
        "the runtime stopped serving after its launcher died"
    );
    assert_eq!(
        journal_generation(&fixture.runtime_dir),
        guardian_before,
        "the guardian was replaced after its launcher died; the runtime was restarted, \
         not preserved"
    );
    Ok(())
}

/// A sidecar that replaces the first one adopts the same runtime instead of a new one.
///
/// A13 says a sidecar restart must not require restarting the whole App: the user's
/// work lives in the runtime, and the replacement sidecar is a new *generation* of
/// the management channel, not a new instance. Three observations make that claim
/// falsifiable rather than rhetorical:
///
/// * the guardian generation is unchanged -- a restarted runtime would mint a new one;
/// * the journal still lists exactly one daemon -- a second bring-up would add one;
/// * the second sidecar authenticates under an epoch the first never used, which is
///   the generation marker the design relies on to retire old capabilities.
///
/// The old capability half of A13 is covered where capabilities actually live: the
/// store refuses entries minted under a previous epoch
/// (`a_capability_from_a_previous_epoch_is_never_accepted`) and retires the old
/// generation on replacement (`a_generation_change_retires_the_old_capability`). This
/// test does not restate that logic; it proves the process-level handover that feeds it.
#[test]
fn a_replacement_sidecar_adopts_the_runtime_without_a_second_bringup() -> anyhow::Result<()> {
    let mut fixture = IsolatedRuntimeFixture::start_without_bringup_env()?;

    // Built up-front rather than in a closure: the two sidecars borrow the fixture
    // mutably, so a closure capturing it would keep the immutable borrow alive.
    let scope = fixture.scope();
    let expected = fixture.artifact().clone();
    let prepare_params = || -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::to_value(
            yi_agent_lifecycle::production::PrepareRequest {
                scope: scope.clone(),
                expected: expected.clone(),
                attempt_id: format!("attempt-{}", uuid_like()),
            },
        )?)
    };

    let mut first = Sidecar::spawn(&mut fixture)?;
    let before = first.call("_shell/prepare", prepare_params()?, START_BUDGET)?;
    anyhow::ensure!(
        before.get("error").is_none()
            && (before.to_string().contains("\"Connected\"")
                || before.to_string().contains("VersionChoiceRequired")),
        "the first sidecar never reached a runtime: {before}"
    );

    let generation_before = journal_generation(&fixture.runtime_dir);
    anyhow::ensure!(
        generation_before.is_some(),
        "the runtime recorded no guardian generation to compare against"
    );
    let daemons_before = count_daemon_records(&fixture.runtime_dir);
    assert_eq!(
        daemons_before, 1,
        "the first prepare left {daemons_before} daemons"
    );

    let first_epoch = first.epoch.clone();
    let first_pid = first.child.id() as i32;
    drop(first);

    // The replacement is a genuinely new channel: new secret, new epoch. Reusing the
    // old ones would make this test unable to tell adoption from a cached session.
    let mut second = Sidecar::spawn(&mut fixture)?;
    assert_ne!(
        second.epoch, first_epoch,
        "the replacement sidecar reused the previous generation's epoch"
    );

    let after = second.call("_shell/prepare", prepare_params()?, START_BUDGET)?;
    anyhow::ensure!(
        after.get("error").is_none(),
        "the replacement sidecar refused a runtime its predecessor had started: {after}"
    );

    // The runtime was adopted, not restarted: same guardian, same single daemon.
    assert_eq!(
        journal_generation(&fixture.runtime_dir),
        generation_before,
        "the replacement sidecar restarted the runtime instead of adopting it"
    );
    assert_eq!(
        count_daemon_records(&fixture.runtime_dir),
        1,
        "a second sidecar brought up a second daemon"
    );
    assert_ne!(
        first_pid,
        second.child.id() as i32,
        "the replacement must be a different process; otherwise nothing was replaced"
    );
    Ok(())
}

/// How many daemon-process records the guardian journal holds.
fn count_daemon_records(runtime_dir: &Path) -> usize {
    journal_records(runtime_dir)
        .iter()
        .filter(|record| {
            record
                .get("owner")
                .and_then(|owner| owner.get("daemon"))
                .is_some()
        })
        .count()
}

/// The guardian generation recorded in the journal, if any.
fn journal_generation(runtime_dir: &Path) -> Option<String> {
    journal_records(runtime_dir).iter().find_map(|record| {
        record
            .get("generation")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    })
}
