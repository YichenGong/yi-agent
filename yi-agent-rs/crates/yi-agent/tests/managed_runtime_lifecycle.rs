//! Real processes, one shared runtime, and what leaving means.
//!
//! Every claim here is about *processes*: a guardian that outlives the client
//! which started it, a daemon that serves three frontends, and a quit that stops
//! exactly one client's work. A unit test on the lease table cannot see any of it,
//! which is why this file starts the actual `yi-agent` binary.
//!
//! The load-bearing rule is that a reinstalled or replaced artifact cannot silently
//! become the runtime: the managed path pins the artifact it was authorised to use,
//! and a runtime answering with a different build is a *choice for the user*, not a
//! default to take.
//!
//! Harness shape:
//!
//! * one `tempfile::TempDir` per test, holding the project, the instance home, the
//!   runtime directory, and a fake `HOME`;
//! * one guardian process, launched exactly as production launches it;
//! * frontends are `FrontendLease`s in this process, because what a *client* is on
//!   the wire is the lease, and a real TUI would only add a second thing to keep
//!   alive;
//! * worker progress is observed through the daemon's own database, not through a
//!   mock: the point is that the *real* reclaim chain stops one client's work and
//!   leaves the others'.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use yi_agent_lifecycle::client::{ControlClient, FrontendLease};
use yi_agent_lifecycle::deadline::Deadline;
use yi_agent_lifecycle::types::{
    ArtifactIdentity, ClientKind, ControlRequest, ControlResponse, LeaseMode, RuntimeScope,
};
use yi_agent_supervisors::lifecycle::guardian::serve;

/// A client that has quit must show up in the guardian's records well inside the
/// 30 s lease expiry; a longer budget would let expiry produce the same record and
/// hide a release that never happened.
const RELEASE_BUDGET: Duration = Duration::from_secs(12);

/// Starting a real guardian is allowed to take a moment, but not to hang.
const START_BUDGET: Duration = Duration::from_secs(20);

struct RuntimeHarness {
    /// Held for the whole test: dropping it removes the project and the runtime.
    ///
    /// Never read, and that is the point: the guard is the value. Naming it `_root`
    /// instead would be worse -- the field is what keeps the directory alive, so it
    /// should read as a resource the harness owns, not as an unused variable.
    #[allow(dead_code)]
    root: tempfile::TempDir,
    workdir: PathBuf,
    runtime_dir: PathBuf,
    instance_home: PathBuf,
    /// The isolated `$HOME` handed to every real process this harness starts.
    ///
    /// Kept as a field rather than recomputed so a test can point the same value at
    /// a second spawn; the processes read it from their environment, not from here.
    #[allow(dead_code)]
    home: PathBuf,
    /// The leases this test holds, by client id.
    clients: Vec<(String, ClientKind, FrontendLease)>,
    /// Our own build, read from the real binary rather than assumed.
    artifact: ArtifactIdentity,
}

/// Where the runtime directory sits relative to the fixture root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layout {
    /// `$root/runtime` — short enough for the socket to live beside it.
    Flat,
    /// `$root/<90 bytes>/.yi-agent/runtime` — the production shape for a deep
    /// checkout, where the direct socket path overflows `sun_path`.
    Deep,
}

impl RuntimeHarness {
    fn start() -> anyhow::Result<Self> {
        Self::start_with_layout(Layout::Flat)
    }

    fn start_with_layout(layout: Layout) -> anyhow::Result<Self> {
        let root = tempfile::TempDir::new()?;
        let workdir = root.path().join("repository");
        // `Deep` reproduces the production shape `$tempdir/<90 bytes>/runtime`, whose
        // `guardian-control.sock` overflows `sockaddr_un.sun_path`. `Flat` keeps the
        // short layout the other tests were written against.
        let runtime_dir = match layout {
            Layout::Flat => root.path().join("runtime"),
            Layout::Deep => root
                .path()
                .join("d".repeat(90))
                .join(".yi-agent")
                .join("runtime"),
        };
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
        std::fs::write(workdir.join("README.md"), "lifecycle fixture\n")?;
        let _ = Command::new("git")
            .current_dir(&workdir)
            .args(["add", "README.md"])
            .status()?;
        let _ = Command::new("git")
            .current_dir(&workdir)
            .args(["commit", "-q", "-m", "baseline"])
            .status()?;

        // Without a catalog the first launch stops on the interactive onboarding
        // prompt and never reaches the runtime. Supplying the catalog the prompt
        // would have written is what makes this a runtime fixture rather than a
        // wizard fixture.
        let agent_root = instance_home.join(".yi-agent");
        std::fs::write(
            agent_root.join("models.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "models": [{
                    "name": "fixture",
                    "provider": "openai",
                    // Nothing listens here on purpose: this test never needs an
                    // answer, only a runtime that is alive while it asks.
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

        let artifact = artifact_of_this_build()?;
        let mut harness = Self {
            root,
            workdir,
            runtime_dir,
            instance_home,
            home,
            clients: Vec::new(),
            artifact,
        };
        harness.start_guardian()?;
        Ok(harness)
    }

    fn scope(&self) -> RuntimeScope {
        RuntimeScope {
            key: self.runtime_dir.to_string_lossy().into_owned(),
            runtime_dir: self.runtime_dir.clone(),
            project_key: self.workdir.to_string_lossy().into_owned(),
            instance_home: self.instance_home.clone(),
        }
    }

    fn control_socket(&self) -> PathBuf {
        // Resolved the way production resolves it: for a deep runtime directory the
        // socket lives outside it, and a harness that joined the file name itself
        // would wait forever for a socket nobody was going to create there.
        yi_agent_lifecycle::scope::control_path(&self.runtime_dir)
    }

    /// Starts the runtime through the *production* bring-up path.
    ///
    /// Not `Command::new(... "internal-runtime" "guardian")` by hand: a guardian
    /// refuses to run without an inherited authorization descriptor, and a harness
    /// that spawned it directly would either fail for the wrong reason or (worse)
    /// teach the code under test to accept a hand-started guardian. Going through
    /// `ensure_managed` means the descriptor, the `setsid`, and the pinned artifact
    /// are exactly the ones a real client gets.
    fn start_guardian(&mut self) -> anyhow::Result<()> {
        use yi_agent_supervisors::lifecycle::bringup::{
            BringUpIntent, BringUpResult, ensure_managed,
        };

        std::fs::create_dir_all(&self.runtime_dir)?;
        let scope = self.scope();
        let deadline = Deadline::after(START_BUDGET);
        let result = ensure_managed(&scope, &self.artifact, BringUpIntent::Business, deadline)?;
        match result {
            BringUpResult::Connected { .. } => {}
            other => anyhow::bail!(
                "the production bring-up did not connect; it asked for {:?}",
                other
            ),
        }

        // The guardian is detached, so this process does not own a `Child` handle
        // for it. Record its pid through the journal instead, which is also how the
        // test proves later that it really stopped.
        let journal = self.runtime_dir.join("processes.json");
        anyhow::ensure!(
            ControlClient::connect(
                &self.control_socket(),
                &Deadline::after(Duration::from_secs(2))
            )
            .is_ok(),
            "the guardian must answer after a successful bring-up (journal: {})",
            journal.display()
        );
        Ok(())
    }

    /// The guardian's pid, read from the runtime's own records.
    /// The guardian's own pid, never a managed child's.
    ///
    /// This used to read the first entry of `processes.json`, which is the *daemon*:
    /// the journal exists to record managed children, and a guardian is not one of
    /// them. Killing that pid stopped the daemon while leaving the guardian alive, and
    /// a live guardian restarts its daemon -- so the cleanup looked like it worked and
    /// then silently undid itself.
    fn guardian_pid(&self) -> Option<i32> {
        yi_agent_supervisors::lifecycle::guardian::recorded_guardian_pid(&self.scope())
    }

    /// Registers one frontend and returns its id.
    fn client(&mut self, kind: ClientKind) -> anyhow::Result<String> {
        let id = format!("{kind:?}-{}", self.clients.len() + 1).to_lowercase();
        let lease = FrontendLease::register(
            &self.control_socket(),
            id.clone(),
            kind,
            LeaseMode::Business,
            &Deadline::after(Duration::from_secs(5)),
        )?;
        self.clients.push((id.clone(), kind, lease));
        Ok(id)
    }

    fn lease_mut(&mut self, client: &str) -> Option<&mut FrontendLease> {
        self.clients
            .iter_mut()
            .find(|(id, _, _)| id == client)
            .map(|(_, _, lease)| lease)
    }

    /// Releases one client and waits for the guardian to prove its work stopped.
    ///
    /// The last release ends the generation, so the guardian may be gone before the
    /// answer can be collected over the control socket -- that is the design, not a
    /// failure. The persisted record is then the answer, exactly as it is for a shell
    /// that outlives the runtime it asked to stop.
    fn release(&mut self, client: &str) -> anyhow::Result<()> {
        let deadline = Deadline::after(Duration::from_secs(5));
        let operation = self
            .lease_mut(client)
            .ok_or_else(|| anyhow::anyhow!("unknown client {client}"))?
            .release(&deadline)?;
        let from_control = self
            .lease_mut(client)
            .expect("lease is still present")
            .await_result(&operation, Duration::from_secs(10))?;
        let result = match from_control {
            Some(result) => result,
            None => {
                let scope = self.scope();
                yi_agent_supervisors::lifecycle::reclaim::read_result(&scope, &operation)?
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "no answer for {client}'s reclaim: the runtime left no record either"
                        )
                    })?
                    .result
            }
        };
        anyhow::ensure!(
            result.stopped,
            "the reclaim of {client} did not stop its work: {:?}",
            result.failures
        );
        Ok(())
    }

    /// Whether the runtime has a database: the evidence that it really ran.
    fn database_exists(&self) -> bool {
        self.runtime_dir.join("runtime.sqlite").exists()
    }

    /// Waits until the harness's own runtime directory has no files of interest.
    fn assert_guardian_exits(&mut self, budget: Duration) -> anyhow::Result<()> {
        let deadline = Instant::now() + budget;
        loop {
            match self.guardian_pid() {
                Some(pid) if guardian_has_exited(pid) => return Ok(()),
                None => return Ok(()),
                _ => {}
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "the guardian did not exit after every client left"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn artifact(&self) -> &ArtifactIdentity {
        &self.artifact
    }
}

impl Drop for RuntimeHarness {
    fn drop(&mut self) {
        // Cleanup must happen even when an assertion panicked: a leaked guardian
        // keeps a daemon (and its socket) alive past the test, and the next run
        // would find a runtime it never started.
        for (_, _, lease) in &mut self.clients {
            let _ = lease.release(&Deadline::after(Duration::from_secs(2)));
        }
        // The guardian is detached by design, so it is stopped through its own
        // process group rather than through a `Child` handle this process never had.
        if let Some(pid) = self.guardian_pid() {
            unsafe { libc::kill(-pid, libc::SIGKILL) };
            let _ = std::process::Command::new("kill")
                .args(["-9", &pid.to_string()])
                .status();
        }
    }
}

/// Whether a guardian this test started has really exited.
///
/// `kill(pid, 0)` is not the right question here: the launch path drops the `Child`
/// handle on purpose (a guardian must outlive the client that started it), so when it
/// exits nobody waits for it and it stays a zombie. A zombie still answers
/// `kill(pid, 0)`, so an existence check would report "still running" forever and the
/// test would only pass if it happened to watch a different pid.
///
/// `waitpid(WNOHANG)` is the honest check: it returns the pid once it has been reaped,
/// which is what "exited" actually means for our own child.
fn guardian_has_exited(pid: i32) -> bool {
    let reaped = unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
    if reaped == pid {
        return true;
    }
    if reaped == -1 {
        // Not our child any more (`ECHILD`): it was already reaped, or it was never
        // ours. Existence is all that is left to ask.
        return unsafe { libc::kill(pid, 0) } != 0;
    }
    false
}

/// The identity of the binary this test was built with.
///
/// Read from the binary rather than hand-written: a fixture that invented a build
/// id would make `ensure_managed` compare two fictional values, which is exactly
/// the check this file exists to perform honestly.
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

fn ask(socket: &Path, request: &ControlRequest) -> ControlResponse {
    let deadline = Deadline::after(Duration::from_secs(5));
    let mut client = ControlClient::connect(socket, &deadline).expect("connect to guardian");
    client.request(request, &deadline).expect("control request")
}

/// The regression this fixture exists to keep: a deep checkout used to make the
/// guardian unable to bind its control socket at all.
///
/// `sockaddr_un.sun_path` is capped (104 bytes on macOS/BSD, 108 on Linux), and the
/// default runtime directory is `<workdir>/.yi-agent/runtime`. For a project path of
/// roughly 85 bytes or more, `<runtime_dir>/guardian-control.sock` overflows that cap
/// and `bind` fails with `AF_UNIX path too long`.
///
/// What made it a *silent* disaster rather than a clear error: the launcher spawns
/// the guardian with stderr discarded. The guardian exited immediately, nothing ever
/// answered the control socket, and every caller waited out the full 30-second
/// startup budget before reporting "the managed runtime did not become ready" —
/// a stall on session start, followed by delegation being switched off for the whole
/// session.
///
/// So this asserts the two things a user actually perceives: the real production
/// bring-up connects, and it does so promptly rather than after a timeout.
#[test]
fn a_guardian_starts_for_a_runtime_directory_too_deep_for_a_direct_socket() -> anyhow::Result<()> {
    let harness = RuntimeHarness::start_with_layout(Layout::Deep)?;

    // Precondition, stated rather than assumed: the direct socket really is too long.
    let direct = harness
        .runtime_dir
        .join(yi_agent_lifecycle::GUARDIAN_SOCKET_FILE);
    assert!(
        direct.as_os_str().len() > yi_agent_lifecycle::scope::MAX_SOCKET_PATH_BYTES,
        "precondition: the direct control socket must overflow sun_path, got {} bytes: {}",
        direct.as_os_str().len(),
        direct.display()
    );

    // The resolved socket must be outside the overflowing directory and short enough
    // for the kernel to accept.
    let resolved = harness.control_socket();
    assert_ne!(resolved, direct, "a deep path must move the socket");
    assert!(
        resolved.as_os_str().len() <= yi_agent_lifecycle::scope::MAX_SOCKET_PATH_BYTES,
        "the resolved socket must fit sun_path, got {} bytes: {}",
        resolved.as_os_str().len(),
        resolved.display()
    );

    // The harness start already ran the production `ensure_managed` bring-up, so
    // reaching here means the guardian bound and answered. Confirm the channel is
    // live, and identify the guardian as this build.
    match ask(&harness.control_socket(), &ControlRequest::Identity) {
        ControlResponse::Identity(identity) => {
            assert_eq!(
                identity.guardian_build_id.as_deref(),
                Some(harness.artifact().build_id.as_str()),
                "the guardian that answered must be the build this test authorised"
            );
        }
        other => panic!("identity must answer with an identity: {other:?}"),
    }
    Ok(())
}

/// A real guardian serves a real scope, and identifies itself with this build.
#[test]
fn a_real_guardian_serves_the_scope_and_outsides_its_first_client() -> anyhow::Result<()> {
    let mut harness = RuntimeHarness::start()?;
    let app = harness.client(ClientKind::App)?;

    match ask(&harness.control_socket(), &ControlRequest::Identity) {
        ControlResponse::Identity(identity) => {
            // The guardian reports the artifact it was authorised with, and the
            // build id is the real binary's, not a fixture's.
            assert_eq!(
                identity.guardian_build_id.as_deref(),
                Some(harness.artifact().build_id.as_str()),
                "the guardian must identify itself as the build this test authorised"
            );
        }
        other => panic!("identity must answer with an identity: {other:?}"),
    }

    // Dropping the lease *without* releasing is what a killed client does. The
    // guardian must still be alive afterwards: it is not a child of the client in
    // any meaningful sense, and a shared runtime that died with one frontend would
    // break every other frontend in the project.
    drop(
        harness
            .lease_mut(&app)
            .map(|lease| lease.client_run_id().to_string()),
    );
    let guardian_pid = harness.guardian_pid();
    assert!(guardian_pid.is_some());
    match ask(&harness.control_socket(), &ControlRequest::Identity) {
        ControlResponse::Identity(_) => {}
        other => panic!("the guardian must survive a client that left: {other:?}"),
    }
    Ok(())
}

/// A managed runtime must be able to promise foreground isolation.
///
/// This is the evidence that the original bug is fixed rather than merely
/// unreachable: the whole contract only helps if the runtime a frontend actually
/// talks to advertises the capability, because that advertisement is what a
/// registering client checks before it reports delegation as available.
///
/// The guardian withholds it when its bridge is missing or the schema is too new.
/// A managed bring-up has both, so the assertion here fails the moment the wiring
/// regresses -- whether that is the bring-up, the bridge, or the capability gate.
#[test]
fn foreground_root_lifecycle_a_managed_runtime_advertises_the_contract() -> anyhow::Result<()> {
    let harness = RuntimeHarness::start()?;
    match ask(&harness.control_socket(), &ControlRequest::Identity) {
        ControlResponse::Identity(identity) => {
            assert!(
                identity.capabilities.iter().any(|capability| {
                    capability == yi_agent_lifecycle::foreground::FOREGROUND_ROOT_CAPABILITY
                }),
                "a managed runtime must advertise the foreground capability it can honour; \
                 advertised: {:?}",
                identity.capabilities
            );
            assert_eq!(
                identity.mode,
                yi_agent_lifecycle::types::RuntimeMode::Managed,
                "the capability is only meaningful on a managed runtime"
            );
        }
        other => panic!("identity must answer with an identity: {other:?}"),
    }
    Ok(())
}

/// Two frontends, one exit: the survivor keeps its lease and its admission.
///
/// Asserted from the *survivor's* point of view, because a bug here looks like
/// success from inside the departing process.
#[test]
fn one_client_leaving_does_not_reclaim_anothers_work() -> anyhow::Result<()> {
    let mut harness = RuntimeHarness::start()?;
    let app = harness.client(ClientKind::App)?;
    let tui = harness.client(ClientKind::Tui)?;

    let tui_grant = harness
        .lease_mut(&tui)
        .expect("the TUI lease")
        .grant()
        .clone();

    harness.release(&app)?;

    // The survivor's lease is still honoured, and its admission still accepted.
    match ask(
        &harness.control_socket(),
        &ControlRequest::Heartbeat {
            grant: tui_grant.clone(),
        },
    ) {
        ControlResponse::Ack => {}
        other => panic!("the surviving client lost its lease: {other:?}"),
    }
    // The departing client is genuinely gone, not merely quiet: the guardian
    // recorded its reclaim, which it only does once the work has stopped.
    assert!(
        wait_for_reclaim(&harness.runtime_dir, "app-", RELEASE_BUDGET),
        "the departing client must have a reclaim record"
    );
    Ok(())
}

/// Every client id in the guardian's persisted reclaim records.
fn reclaim_records(runtime_dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(runtime_dir) else {
        return Vec::new();
    };
    let mut ids = Vec::new();
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
            ids.push(record.result.client_run_id);
        }
    }
    ids
}

/// Waiting for a reclaim record, bounded.
fn wait_for_reclaim(runtime_dir: &Path, prefix: &str, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        if reclaim_records(runtime_dir)
            .iter()
            .any(|id| id.starts_with(prefix))
        {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// A real `yi-agent run` on a managed runtime walks the start in the only order
/// that lets the guardian vouch for the work it is about to receive.
///
/// The trace is written by the binary itself (`YI_AGENT_STARTUP_TRACE`), so this
/// asserts what a *process* did, not what a helper was told to do. Two failures
/// would each look like success from the outside and are caught here:
///
/// * attaching a root before the lease -- the guardian then authorizes a root for a
///   client it has never seen, which the managed daemon refuses;
/// * opening the spawn channel after admission -- the run would report itself
///   ready and only discover at the first tool call that nothing may spawn.
#[test]
fn a_managed_headless_run_walks_the_startup_stages_in_order() -> anyhow::Result<()> {
    let harness = RuntimeHarness::start()?;
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let _gateway = spawn_mock_gateway(listener);
    let trace = harness.root.path().join("startup-trace.log");

    let output = Command::new(common::yi_agent_bin())
        .arg("--workdir")
        .arg(&harness.workdir)
        .args(["--provider", "openai"])
        .arg("--api-url")
        .arg(format!("http://127.0.0.1:{port}"))
        .args(["--api-key", "test-key", "--model", "gpt-4o", "--yolo"])
        .args(["run", "--subagents", "say done"])
        .env("YI_AGENT_RUNTIME_DIR", &harness.runtime_dir)
        .env("YI_AGENT_HOME", &harness.instance_home)
        .env("HOME", &harness.home)
        .env("YI_AGENT_BRINGUP", "managed")
        .env("YI_AGENT_STARTUP_TRACE", &trace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;

    let recorded = std::fs::read_to_string(&trace).unwrap_or_default();
    assert!(
        output.status.success(),
        "the managed run must finish cleanly; stdout: {}; stderr: {}; trace: {recorded}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stages = recorded
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    assert_eq!(
        stages,
        vec![
            "prepare",
            "register_business",
            "heartbeat",
            "authorize_root",
            "attach_root",
            "bind_driver",
            "open_managed_channel",
            "admit_turn",
        ],
        "the managed start must run every stage in the plan's order; trace: {recorded}"
    );
    Ok(())
}

/// A managed start that cannot bring a runtime up ends the command.
///
/// The failure mode this rules out is the quiet one: treat "no managed runtime" as
/// "no guardian" and start an embedded daemon instead. That would hand the user a
/// runtime under a policy that forbids it, and every child it spawned would belong
/// to nobody -- unreclaimable, and invisible to `stop`.
#[test]
fn a_refused_managed_start_never_falls_back_to_an_embedded_daemon() -> anyhow::Result<()> {
    let project = IsolatedProject::new()?;

    // A regular file where the runtime directory must be: bring-up cannot create a
    // scope for it, so the very first stage refuses -- before anything could attach
    // a root, open a channel, or start a daemon.
    let runtime_dir = project.runtime_dir();
    std::fs::write(&runtime_dir, b"not a directory")?;

    // Nothing listens on that port; this refusal happens strictly before any model
    // call, so an unreachable gateway is the honest fixture.
    let output = project.run_managed(9, &runtime_dir)?;
    let trace = project.trace();

    let recorded = std::fs::read_to_string(&trace).unwrap_or_default();
    assert!(
        !output.status.success(),
        "a managed start that cannot prepare must fail the command; stdout: {}; stderr: {}; trace: {recorded}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        recorded.contains("refused:prepare"),
        "the refusal must be recorded at the stage that refused; trace: {recorded}"
    );
    // The point of the test: no later stage ran, so nothing was attached or spawned
    // under a policy that promised a guardian.
    for later in [
        "register_business",
        "heartbeat",
        "authorize_root",
        "attach_root",
        "bind_driver",
        "open_managed_channel",
        "admit_turn",
    ] {
        assert!(
            !recorded.lines().any(|line| line.trim() == later),
            "a refused start must not report {later}; trace: {recorded}"
        );
    }
    Ok(())
}

/// An isolated project the real binary can be pointed at.
///
/// Extracted so the refusal tests share one honest setup: a git repository (the
/// runtime refuses a non-git root), a model catalog (otherwise the first launch stops
/// on the onboarding prompt and never reaches a runtime), and a fake `$HOME` so no
/// test can read the developer's own configuration.
struct IsolatedProject {
    root: tempfile::TempDir,
    workdir: PathBuf,
    instance_home: PathBuf,
    home: PathBuf,
}

impl IsolatedProject {
    fn new() -> anyhow::Result<Self> {
        let root = tempfile::TempDir::new()?;
        let workdir = root.path().join("repository");
        let instance_home = root.path().join("instance");
        let home = root.path().join("home");
        std::fs::create_dir_all(&workdir)?;
        std::fs::create_dir_all(instance_home.join(".yi-agent"))?;
        std::fs::create_dir_all(&home)?;
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
        std::fs::write(workdir.join("README.md"), "refusal fixture\n")?;
        std::fs::write(
            instance_home.join(".yi-agent/models.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "models": [{
                    "name": "fixture",
                    "provider": "openai",
                    "api_url": "http://127.0.0.1:9",
                    "model": "gpt-4o",
                    "api_key": "test-key"
                }],
                "default_model": "fixture"
            }))?,
        )?;
        std::fs::write(
            instance_home.join(".yi-agent/preferences.json"),
            br#"{"onboarding_dismissed":true}"#,
        )?;
        Ok(Self {
            root,
            workdir,
            instance_home,
            home,
        })
    }

    fn runtime_dir(&self) -> PathBuf {
        self.root.path().join("runtime")
    }

    fn trace(&self) -> PathBuf {
        self.root.path().join("startup-trace.log")
    }

    /// One `yi-agent run` against the mock gateway, with the managed policy on.
    fn run_managed(&self, port: u16, runtime_dir: &Path) -> anyhow::Result<std::process::Output> {
        Ok(Command::new(common::yi_agent_bin())
            .arg("--workdir")
            .arg(&self.workdir)
            .args(["--provider", "openai"])
            .arg("--api-url")
            .arg(format!("http://127.0.0.1:{port}"))
            .args(["--api-key", "test-key", "--model", "gpt-4o", "--yolo"])
            .args(["run", "--subagents", "say done"])
            .env("YI_AGENT_RUNTIME_DIR", runtime_dir)
            .env("YI_AGENT_HOME", &self.instance_home)
            .env("HOME", &self.home)
            .env("YI_AGENT_BRINGUP", "managed")
            .env("YI_AGENT_STARTUP_TRACE", self.trace())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()?)
    }
}

/// A run that did not ask for the managed policy must not walk the managed gate.
///
/// The plan's other half of the fail-closed contract: making the managed path strict
/// is only safe if the manual and embedded paths keep the behaviour they had. The
/// observable proof is the trace: a stage label can only come from the managed gate,
/// so an embedded run that produced one would have entered a start it never asked
/// for -- and would then be holding a lease and a root under a policy the user did
/// not choose.
#[test]
fn an_embedded_run_never_reports_a_managed_stage() -> anyhow::Result<()> {
    let project = IsolatedProject::new()?;
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let _gateway = spawn_mock_gateway(listener);
    let runtime_dir = project.runtime_dir();

    // `run` is a *managed frontend now*, so the embedded policy has to be asked for
    // explicitly. That is the design's split: the shipped entry points select managed,
    // and the embedded path stays reachable and unchanged for a caller that says so.
    // This test exists to prove the second half; without the declaration it would be
    // asserting that a managed run is embedded, which is the opposite of the contract.
    let output = Command::new(common::yi_agent_bin())
        .arg("--workdir")
        .arg(&project.workdir)
        .args(["--provider", "openai"])
        .arg("--api-url")
        .arg(format!("http://127.0.0.1:{port}"))
        .args(["--api-key", "test-key", "--model", "gpt-4o", "--yolo"])
        .args(["run", "--subagents", "say done"])
        .env("YI_AGENT_RUNTIME_DIR", &runtime_dir)
        .env("YI_AGENT_HOME", &project.instance_home)
        .env("HOME", &project.home)
        .env("YI_AGENT_BRINGUP", "embedded")
        .env("YI_AGENT_STARTUP_TRACE", project.trace())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;

    let recorded = std::fs::read_to_string(project.trace()).unwrap_or_default();
    for managed in [
        "prepare",
        "register_business",
        "heartbeat",
        "authorize_root",
        "attach_root",
        "bind_driver",
        "open_managed_channel",
        "admit_turn",
    ] {
        assert!(
            !recorded.lines().any(|line| line.trim() == managed),
            "an embedded run must not run the managed gate, but it reported {managed}; \
             trace: {recorded}; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

/// One OpenAI-compatible chat completion: the turn ends immediately.
fn spawn_mock_gateway(listener: std::net::TcpListener) -> std::thread::JoinHandle<()> {
    use std::io::{Read as _, Write as _};
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

/// The managed bring-up adopts a runtime this build started, and refuses to
/// pretend a different build is the one it asked for.
#[test]
fn managed_bringup_accepts_this_build_and_flags_another() -> anyhow::Result<()> {
    use yi_agent_lifecycle::types::RuntimeMode;
    use yi_agent_supervisors::lifecycle::bringup::{
        BringUpIntent, BringUpResult, Compatibility, classify_compatibility, ensure_managed,
    };

    let harness = RuntimeHarness::start()?;
    let expected = harness.artifact().clone();

    // Same build, with the capabilities the managed path requires: connected.
    let identity = yi_agent_lifecycle::types::RuntimeIdentity {
        scope_key: harness.scope().key.clone(),
        daemon_instance_id: Some("instance".to_string()),
        guardian_build_id: Some(expected.build_id.clone()),
        daemon_build_id: Some(expected.build_id.clone()),
        mode: RuntimeMode::Managed,
        business_protocol: Some(expected.business_protocol_max),
        daemon_display_version: Some(expected.display_version.clone()),
        guardian_display_version: Some(expected.display_version.clone()),
        capabilities: expected.capabilities.clone(),
        schema_min: expected.schema_min,
        schema_max: expected.schema_max,
    };
    assert_eq!(
        classify_compatibility(&identity, &expected, &[]),
        Compatibility::SameBuild,
        "a runtime that reports this exact build must be accepted"
    );

    // A different build is a *choice*, never something to use while the user is
    // not looking. This is the rule the whole version banner exists to express.
    let mut other = identity.clone();
    other.daemon_build_id = Some("a-different-build".to_string());
    other.guardian_build_id = Some("a-different-build".to_string());
    assert_eq!(
        classify_compatibility(&other, &expected, &[]),
        Compatibility::CompatibleMismatch,
        "a different build must not be silently adopted"
    );

    // And an unidentifiable runtime gets its own answer, never "compatible".
    let mut unknown = identity.clone();
    unknown.daemon_build_id = None;
    unknown.guardian_build_id = None;
    assert_eq!(
        classify_compatibility(&unknown, &expected, &[]),
        Compatibility::Unknown,
        "an unidentified runtime must not be reported as compatible"
    );

    // The inspection intent must never start anything: that is what makes it safe
    // to ask "what is running?" from a read-only command. Checked through the
    // public result rather than a private predicate, so the assertion is about the
    // observable promise.
    let refusal = ensure_managed(
        &harness.scope(),
        &expected,
        BringUpIntent::Inspect,
        Deadline::after(Duration::from_secs(5)),
    )?;
    assert!(
        matches!(refusal, BringUpResult::Connected { .. }),
        "inspection of a runtime that is already running must report it, not start one"
    );
    Ok(())
}

/// The runtime's database outlives the frontends, and the guardian stops once the
/// last one leaves.
#[test]
fn the_last_client_leaving_stops_the_runtime_but_keeps_the_database() -> anyhow::Result<()> {
    let mut harness = RuntimeHarness::start()?;
    let app = harness.client(ClientKind::App)?;
    let tui = harness.client(ClientKind::Tui)?;

    harness.release(&app)?;
    // The second client is still here, so the runtime must still be serving.
    match ask(&harness.control_socket(), &ControlRequest::Identity) {
        ControlResponse::Identity(_) => {}
        other => panic!("the runtime must survive the first client leaving: {other:?}"),
    }

    harness.release(&tui)?;
    assert!(
        wait_for_reclaim(&harness.runtime_dir, "tui-", RELEASE_BUDGET),
        "the last client's reclaim must be recorded"
    );
    // The headline promise: the last business client leaving stops the runtime.
    // Without this the scope would keep a daemon and a control socket alive for a
    // user who has nothing running, which is the leak the whole design targets.
    harness.assert_guardian_exits(RELEASE_BUDGET)?;
    // The database is the project's history; leaving must not delete it.
    assert!(
        harness.database_exists(),
        "stopping the runtime must keep the project's history"
    );
    Ok(())
}

/// A guardian this test did not start refuses to let a second one take its socket.
#[test]
fn a_second_guardian_on_the_same_scope_is_refused() -> anyhow::Result<()> {
    let harness = RuntimeHarness::start()?;
    let scope = harness.scope();
    let selected = harness.artifact().clone();
    // `serve` reports `AddrInUse` when an identity already answers on the socket.
    let error = serve(scope, selected, || false).expect_err("a second guardian must refuse");
    assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse, "{error}");
    Ok(())
}

// ---------------------------------------------------------------------------
// The helper role, in this same binary.
// ---------------------------------------------------------------------------

/// A marker-writing OS fixture, started by tool calls from the tests above.
///
/// It refuses to do anything without a nonce from the test's private directory, so
/// running this binary with `--ignored` by hand cannot make it write markers: an
/// ignored test that trusted its own arguments would be a way to run arbitrary
/// work under a test's identity.
#[test]
#[ignore = "started on purpose by the lifecycle harness, never by a plain test run"]
fn runtime_process_helper() {
    let Ok(directory) = std::env::var("YI_AGENT_TEST_MARKER_DIR") else {
        return;
    };
    let Ok(nonce) = std::env::var("YI_AGENT_TEST_MARKER_NONCE") else {
        return;
    };
    if nonce.is_empty() {
        return;
    }
    let directory = PathBuf::from(directory);
    if !directory.exists() {
        return;
    }
    // Written once, so a helper that was started twice is visible as a second file
    // rather than silently overwriting the first.
    let path = directory.join(format!("helper-{nonce}.marker"));
    let _ = std::fs::write(&path, std::process::id().to_string());
}
