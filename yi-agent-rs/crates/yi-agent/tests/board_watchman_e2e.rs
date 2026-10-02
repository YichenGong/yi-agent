//! End-to-end acceptance for the board-daemon watchman (plan §5 of
//! `docs/superpowers/specs/2026-10-02-board-daemon-watchman-design.md`).
//!
//! These tests drive the **real** binaries against a temp `HOME`, exactly like
//! `board_e2e.rs`: a real `yi-agent app-server` over JSONL JSON-RPC on stdio,
//! real detached `daemon serve` processes, and the real plugin. What a unit test
//! cannot reach is what this pins: that the watchman actually revives a killed
//! daemon by reading the *generic* resident registry, and that an orphaned
//! plugin yields the advance right instead of holding it forever.
//!
//! # Why the gate
//!
//! The whole flow needs the plugin binary and real (detached) child processes,
//! so it is opt-in: set `YI_AGENT_BOARD_E2E=1`. Without that (or without a
//! plugin the scaffold can resolve) each test reports why and returns green — a
//! missing *external* tool must not fail the suite. Run it with:
//!
//! ```text
//! YI_AGENT_BOARD_E2E=1 cargo test -p yi-agent --test board_watchman_e2e -- --nocapture
//! ```
//!
//! # Launchd is deliberately never touched
//!
//! Neither test installs a LaunchAgent. The `app-server` this test spawns is
//! started with `YI_AGENT_DISABLE_WATCHMAN=1`, the production kill-switch added
//! for exactly this (see `production_watchman_install`): `board/create` would
//! otherwise install the launchd job on first use and `bootstrap` it into the
//! developer's real launchd domain. The watchman is exercised by running its
//! **loop body once** (`yi-agent boards watch --interval-secs 1`, i.e.
//! `watch::once` + `ensure_daemons`) rather than via a real launchd job, which
//! is precisely what the acceptance item is about — "the daemon comes back" —
//! minus the launchd plumbing that Task 5/6 unit tests already cover.
//!
//! # Process identification
//!
//! Both assertions hinge on telling the daemon and the plugin apart. There is no
//! pid file to read (deliberately: `runtime.lock` / `plugin.lock` are advisory
//! `flock`s, so ownership is decided by the kernel and released on `SIGKILL`).
//! We therefore ask `lsof -t` which process holds each lock file. That is the
//! same kernel state the product relies on.
//!
//! `lsof` alone is not enough for cleanup: the daemon's command line carries no
//! project path (`yi-agent daemon serve` runs with the project as its cwd), so a
//! sweep that only asks "who holds `runtime.lock`" cannot see the daemon if the
//! lock file is missing or `lsof` reports nothing. Cleanup therefore also
//! identifies the daemon by the `HOME=<temp home>` token in its environment
//! (macOS `ps -E`) and the plugin by its argv, then *asserts* the locks came
//! back empty rather than trusting the kill.
//!
//! # What the tests do and do not claim
//!
//! The second `plugin run` in [`an_orphan_plugin_exits_after_the_daemon_dies`] is
//! observed to stay alive yet never appear as a lock holder: that is a liveness /
//! identity check, not an in-process `flock` contention proof (which lives in the
//! plugin's `single_instance::tests::a_second_instance_cannot_take_the_lock`).
//! Neither test ever starts a card, so neither observes card-level de-duplication
//! directly; "no card is launched twice" is inferred from the single-advancer
//! lock plus the plugin's queued-launch unit tests, and is stated as such in the
//! spec's evidence table.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// The plugin executable a production board's scaffold resolves to (Homebrew
/// first, then `PATH`). Mirrors `board_e2e::PLUGIN_BIN`. Only
/// [`killing_a_board_daemon_is_healed_by_the_watchman`] depends on it, because
/// only that test drives the real `board/create` scaffolding path.
const PUBLISHED_PLUGIN_BIN: &str = "/opt/homebrew/bin/superpowers-kanban";

/// How long `board/create` may take to report a ready daemon (mirrors
/// `board_e2e`, whose bound is the daemon's own `READY_TIMEOUT`).
const CREATE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a killed daemon may take to come back. The watch loop ticks every
/// second here, so the real wait is ~1–2s; the bound only guards a wedged start.
const HEAL_TIMEOUT: Duration = Duration::from_secs(30);

/// How long an orphaned plugin may hold the lock after its daemon dies. With
/// `--interval-secs 1` the liveness threshold (3 misses) fires at ~3s; the
/// bound leaves room for a loaded machine.
const ORPHAN_EXIT_TIMEOUT: Duration = Duration::from_secs(45);

/// Polling cadence for every bounded wait. No fixed sleep is used for
/// correctness: each wait polls its condition against a deadline.
const POLL: Duration = Duration::from_millis(100);

/// The plugin built in this worktree. Task 8 (the single-instance lock and the
/// daemon-liveness exit) lives in this source tree; the *installed* Homebrew
/// copy on this machine predates it, so the orphan test must run the worktree
/// build to observe the behaviour under test. Built with:
/// `cargo build -p superpowers-kanban-runner --bin superpowers-kanban`.
fn worktree_plugin_bin() -> PathBuf {
    PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../plugins/superpowers-kanban/target/debug/superpowers-kanban"
    ))
}

fn yi_agent_bin() -> PathBuf {
    std::env::var_os("CARGO_BIN_EXE_yi-agent")
        .map(Into::into)
        .expect("Cargo must provide the yi-agent binary")
}

/// The opt-in gate common to both tests. `Some(reason)` means "report and skip".
///
/// The Homebrew check is *not* here: only the test that drives `board/create`
/// needs the installed plugin (its scaffold resolves the Homebrew binary), so it
/// lives in [`needs_published_plugin`]. The orphan test builds and runs the
/// worktree plugin and must be able to run on a machine without Homebrew.
fn gate_reason() -> Option<String> {
    if std::env::var_os("YI_AGENT_BOARD_E2E").is_none() {
        return Some("set YI_AGENT_BOARD_E2E=1 to run the watchman end-to-end tests".into());
    }
    None
}

/// The per-test addition to [`gate_reason`] for tests that go through the real
/// `board/create` scaffolding.
fn needs_published_plugin() -> Option<String> {
    if !Path::new(PUBLISHED_PLUGIN_BIN).is_file() {
        return Some(format!("{PUBLISHED_PLUGIN_BIN} is not installed"));
    }
    None
}

// ---------------------------------------------------------------------------
// A live `yi-agent app-server` speaking JSONL JSON-RPC on stdio.
// (Same technique as `board_e2e.rs`; kept self-contained on purpose so the two
// integration tests can evolve independently.)
// ---------------------------------------------------------------------------

struct AppServer {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl AppServer {
    fn start(bin: &Path, home: &Path) -> AppServer {
        let mut child = Command::new(bin)
            .args(["app-server", "--listen", "stdio://"])
            .env("HOME", home)
            // Never boot a host LaunchAgent from a test. `board/create` installs
            // the watchman on first use; the switch degrades that production
            // install/uninstall to a no-op (see `production_watchman_install`).
            .env("YI_AGENT_DISABLE_WATCHMAN", "1")
            // A bundled app launches from `/`; mirror that so an accidental
            // reliance on the cwd shows up here rather than in production.
            .current_dir("/")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn yi-agent app-server");
        let stdin = child.stdin.take().expect("app-server stdin");
        let stdout = BufReader::new(child.stdout.take().expect("app-server stdout"));
        AppServer {
            child,
            stdin: Some(stdin),
            stdout,
            next_id: 0,
        }
    }

    fn rpc(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let stdin = self.stdin.as_mut().expect("app-server stdin open");
        writeln!(stdin, "{request}").expect("write request");
        stdin.flush().expect("flush request");
        loop {
            let mut line = String::new();
            let read = self.stdout.read_line(&mut line).expect("read response");
            assert!(read > 0, "app-server closed before answering `{method}`");
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if value.get("id") == Some(&json!(id)) {
                return value;
            }
        }
    }

    fn rpc_within(&mut self, method: &str, params: Value, timeout: Duration) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            let value = self.rpc(method, params.clone());
            if value.get("error").is_none() || Instant::now() >= deadline {
                return value;
            }
            std::thread::sleep(POLL);
        }
    }

    fn initialize(&mut self) {
        let value = self.rpc("initialize", json!({}));
        assert!(value.get("error").is_none(), "initialize failed: {value}");
    }

    fn shutdown(mut self) -> std::process::ExitStatus {
        drop(self.stdin.take());
        self.child.wait().expect("wait for app-server")
    }
}

impl Drop for AppServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------
// Generic cleanup + process probing helpers.
// ---------------------------------------------------------------------------

/// Kills each pid on drop. Used for processes this test does **not** own (a
/// detached daemon, an orphaned plugin): the test cannot `wait()` them, but it
/// must still not leak them into the developer's machine.
#[derive(Default)]
struct KillOnDrop(Vec<i32>);

impl KillOnDrop {
    fn add(&mut self, pid: i32) -> i32 {
        self.0.push(pid);
        pid
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        for pid in &self.0 {
            unsafe {
                libc::kill(*pid, libc::SIGKILL);
            }
        }
    }
}

/// A child this test started directly; killed and reaped on drop.
struct ChildGuard(Child);

impl ChildGuard {
    fn spawn(mut command: Command) -> ChildGuard {
        let child = command.spawn().expect("spawn child process");
        ChildGuard(child)
    }

    fn pid(&self) -> i32 {
        self.0.id() as i32
    }

    fn kill(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Sweeps every process tied to a project on drop, so even a panicking assertion
/// cannot leak a fixture daemon or plugin onto the developer's machine. Declared
/// at the top of each test, so it runs after the process guards and the tempdir
/// are torn down. Best-effort only: the explicit path in each test also asserts
/// the sweep drained the locks (see [`verify_no_project_processes`]).
struct ProjectSweeper(PathBuf);

impl Drop for ProjectSweeper {
    fn drop(&mut self) {
        kill_project_processes(&self.0);
    }
}

/// Processes holding `path` open, via `lsof -t`. This is how we name the daemon
/// (it holds `runtime.lock`) and the advancing plugin (it holds `plugin.lock`).
fn lock_holders(path: &Path) -> Vec<i32> {
    let output = match Command::new("/usr/sbin/lsof").arg("-t").arg(path).output() {
        Ok(output) => output,
        Err(error) => panic!("could not run lsof to identify holders of {path:?}: {error}"),
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<i32>().ok())
        .collect()
}

fn runtime_dir(project: &Path) -> PathBuf {
    project.join(".yi-agent").join("runtime")
}

/// The per-project queue directory the plugin's single-instance lock lives in;
/// matches `yi-agent-supervisors::supervisor::Layout::for_workdir`.
fn state_dir(project: &Path) -> PathBuf {
    project.join(".yi-agent").join("superpowers-kanban")
}

/// The daemon serving `project`: the process holding the runtime instance lock.
fn daemon_pids(project: &Path) -> Vec<i32> {
    lock_holders(&runtime_dir(project).join("runtime.lock"))
}

/// The plugin(s) advancing `project`'s queue: holder(s) of the single-instance
/// lock.
fn plugin_pids(project: &Path) -> Vec<i32> {
    lock_holders(&state_dir(project).join("plugin.lock"))
}

fn pid_alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// One `ps` row: the pid and everything after it (command line *and* `-E`'s
/// environment blob). `-E` puts the environment after the command, and the split
/// between the two is not worth reconstructing: matching the combined text is
/// enough to see both a plugin's project-bearing argv and any process's
/// `HOME=<temp home>` env token.
struct PsRow {
    pid: i32,
    /// Everything after the pid: command line followed by `VAR=value ...`.
    rest: String,
}

/// Every visible process's pid + command + environment. `-E` is how macOS
/// exposes the env (there is no `-o env=` keyword); `-A` lists all processes,
/// `-w -w` defeats the width truncation that would clip a long temp path out of
/// the line.
fn ps_rows() -> Vec<PsRow> {
    let output = Command::new("/bin/ps")
        .args(["-A", "-w", "-w", "-o", "pid=,command=", "-E"])
        .output()
        .expect("run ps to identify project processes");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let rest = line.trim_start();
            let (pid, after_pid) = rest.split_once(char::is_whitespace)?;
            Some(PsRow {
                pid: pid.parse::<i32>().ok()?,
                rest: after_pid.to_string(),
            })
        })
        .collect()
}

/// Poll `condition` until it holds or `timeout` elapses. Never a fixed sleep:
/// the caller asserts the *outcome*, the poll only avoids racing a slow step.
fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if condition() {
            return true;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        std::thread::sleep(POLL.min(remaining));
    }
}

/// Best-effort sweep: `SIGKILL` every process belonging to `project` — the
/// daemon(s), the plugin(s) (including an orphan whose daemon was `SIGKILL`ed and
/// which has not yet noticed), and (as a fallback) the `boards watch` loop. Never
/// asserts: it also runs from `Drop`, so it must not panic while unwinding.
///
/// A test must never leave a fixture process on the developer's machine, so the
/// daemon cannot be identified by `lsof` alone. Its command line carries no
/// project path (`yi-agent daemon serve` runs with the project as its cwd), so
/// three independent signals are combined:
///   1. `lsof -t` on `runtime.lock` / `plugin.lock` (kernel state, the same the
///      product relies on);
///   2. the plugin's argv, which embeds the project's runtime/state/root paths
///      (so even an orphan that still holds the lock is found);
///   3. the `HOME=<temp home>` token in a process's environment (`ps -E`), which
///      for this test is the app-server's temp home and therefore identifies the
///      daemon regardless of whether `lsof` reported it.
///
/// [`verify_no_project_processes`] turns this into a real guarantee.
fn kill_project_processes(project: &Path) {
    // The app-server is always started with `HOME=<temp home>`, and the daemon it
    // (or any second launcher) spawns inherits it. Using it as a signal means the
    // daemon is found even with the lock file missing.
    let home_token = format!(
        "HOME={}",
        project
            .parent()
            .map(|dir| dir.join("home").to_string_lossy().to_string())
            .unwrap_or_default()
    );
    let needle = project.to_string_lossy().to_string();

    let mut victims: Vec<i32> = daemon_pids(project);
    victims.extend(plugin_pids(project));
    for row in ps_rows() {
        let owns_project = row.rest.contains(&needle) && row.rest.contains("superpowers-kanban");
        let owns_temp_home = row.rest.contains(&home_token);
        if owns_project || owns_temp_home {
            victims.push(row.pid);
        }
    }
    for pid in victims {
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
}

/// Assert the sweep really drained `project`: nothing still holds its runtime or
/// plugin lock. Called explicitly, never from `Drop`, so a leak fails the test
/// instead of being claimed as handled.
fn verify_no_project_processes(project: &Path) {
    assert!(
        wait_until(Duration::from_secs(10), || {
            daemon_pids(project).is_empty() && plugin_pids(project).is_empty()
        }),
        "the sweep must leave no process holding {project:?}'s runtime/plugin locks"
    );
}

/// Spawn `yi-agent boards watch --interval-secs 1` against `home`.
///
/// This is the watchman loop body: every tick it reads the generic resident
/// registry (`resident-daemons.json`) and calls `ensure_daemons`. The unit of
/// acceptance is a single tick, so a 1s interval makes the heal observable
/// quickly without waiting out the production 30s cadence.
fn spawn_boards_watch(bin: &Path, home: &Path) -> ChildGuard {
    let mut command = Command::new(bin);
    command
        .args(["boards", "watch", "--interval-secs", "1"])
        .env("HOME", home)
        // `boards watch` does not touch launchd today, but a test that runs the
        // watchman must never be able to bootstrap a host LaunchAgent, so set
        // the same kill-switch the app-server gets.
        .env("YI_AGENT_DISABLE_WATCHMAN", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    ChildGuard::spawn(command)
}

/// Write the same project scaffolding a board would get, but pointing the
/// supervisor at `plugin` and ticking every second so the liveness threshold
/// (3 misses) fires in ~3s instead of ~30s.
fn scaffold_fast_project(project: &Path, plugin: &Path) {
    let supervisors = project.join(".yi-agent").join("supervisors");
    std::fs::create_dir_all(&supervisors).unwrap();
    std::fs::write(
        project.join(".yi-agent").join("preferences.json"),
        "{\"superpowers_kanban\": true}\n",
    )
    .unwrap();
    let manifest = json!({
        "name": "superpowers-kanban",
        "command": plugin.to_string_lossy(),
        "args": [
            "run",
            "--runtime-dir", "{runtime_dir}",
            "--state-dir", "{state_dir}",
            "--project-root", "{workdir}",
            "--interval-secs", "1"
        ],
        "switch_key": "superpowers_kanban",
        "stop_when_disabled": false,
        "restart_backoff_ms": 1000,
        "restart_backoff_max_ms": 30000,
        "query_socket": "{state_dir}/superpowers-kanban.sock"
    });
    std::fs::write(
        supervisors.join("superpowers-kanban.json"),
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .unwrap();
}

/// Start a real `daemon serve` for `project`, rooted at the project so its
/// socket/lock land under the project's `.yi-agent/runtime`.
fn spawn_daemon(bin: &Path, project: &Path, home: &Path) -> ChildGuard {
    let mut command = Command::new(bin);
    command
        .args(["daemon", "serve"])
        .current_dir(project)
        .env("HOME", home)
        .env("YI_AGENT_DISABLE_WATCHMAN", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    ChildGuard::spawn(command)
}

/// Poll until a daemon for `project` is answering and we know its pid, or fail.
fn wait_for_daemon(project: &Path, timeout: Duration) -> i32 {
    let deadline = Instant::now() + timeout;
    loop {
        if yi_agent_boards::board_daemon::is_running(project) {
            if let Some(pid) = daemon_pids(project).first() {
                return *pid;
            }
        }
        assert!(
            Instant::now() < deadline,
            "no daemon for {} became answerable within {timeout:?}",
            project.display()
        );
        std::thread::sleep(POLL);
    }
}

/// Poll until a plugin holds `project`'s single-instance lock, or fail.
fn wait_for_plugin(project: &Path, timeout: Duration) -> i32 {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(pid) = plugin_pids(project).first() {
            return *pid;
        }
        assert!(
            Instant::now() < deadline,
            "no plugin took the lock at {} within {timeout:?}",
            state_dir(project).join("plugin.lock").display()
        );
        std::thread::sleep(POLL);
    }
}

// ---------------------------------------------------------------------------
// Test 1 — a killed board daemon is healed by the watchman.
// ---------------------------------------------------------------------------

/// Acceptance §5.1/§5.3: kill a project's daemon; the watchman brings it back
/// within a bounded time, and there is never more than one daemon per project.
///
/// Faithfulness note: real launchd is not driven here (the app-server runs with
/// `YI_AGENT_DISABLE_WATCHMAN=1`, and installing a host LaunchAgent is precisely
/// what the gate exists to prevent). Instead we run the watchman's loop body —
/// `boards watch --interval-secs 1`, i.e. `watch::once` → `ensure_daemons` — as
/// a plain detached process, which is the same logic launchd's `KeepAlive`
/// hosts. What launchd adds (re-launching the *watchman* itself after a reboot)
/// is a Task 5/6 unit concern; the invariant this test owns is "a registered
/// project's daemon comes back", which it drives end-to-end with real
/// processes.
#[test]
fn killing_a_board_daemon_is_healed_by_the_watchman() {
    if let Some(reason) = gate_reason().or_else(needs_published_plugin) {
        eprintln!("skipping watchman end-to-end: {reason}");
        return;
    }

    let root = tempfile::TempDir::new().unwrap();
    let home = root.path().join("home");
    let project = root.path().join("project");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    // The server canonicalizes registered project paths; compare like for like.
    let project = project.canonicalize().unwrap();
    let project_str = project.to_string_lossy().to_string();
    let resident_dir = home.join(".yi-agent");
    let bin = yi_agent_bin();
    let mut kills = KillOnDrop::default();
    let _sweeper = ProjectSweeper(project.clone());

    // (1) A real board: registry + manifest + switch, and a real daemon.
    let mut server = AppServer::start(&bin, &home);
    server.initialize();
    let created = server.rpc_within(
        "board/create",
        json!({"project": project_str}),
        CREATE_TIMEOUT,
    );
    assert!(
        created.get("error").is_none(),
        "board/create failed: {created}"
    );
    assert_eq!(
        created["result"]["daemon_running"], true,
        "the created board must have a running daemon: {created}"
    );

    // Creation must have declared a *generic* resident need — the watchman's
    // only input. This is the coupling acceptance §5.5 asks about.
    assert_eq!(
        yi_agent_store::resident::list(&resident_dir),
        vec![project.clone()],
        "board/create must register a resident-daemon need for this project"
    );

    let daemon = wait_for_daemon(&project, CREATE_TIMEOUT);
    kills.add(daemon);

    // (2) The daemon outlives the app-server, and the app-server is stopped so
    // the *watchman* — not the app's own self-heal loop — is the only thing that
    // can bring the daemon back.
    let status = server.shutdown();
    assert!(status.success(), "app-server exited abnormally: {status}");
    assert!(
        yi_agent_boards::board_daemon::is_running(&project),
        "the detached daemon must keep answering after the app-server exits"
    );

    // (3) Kill the daemon hard. There is no pid file; the kernel releases the
    // flock when the process dies, and the socket stops answering.
    unsafe {
        libc::kill(daemon, libc::SIGKILL);
    }
    assert!(
        wait_until(Duration::from_secs(10), || {
            !yi_agent_boards::board_daemon::is_running(&project)
        }),
        "the killed daemon must stop answering before we can test the heal"
    );

    // (4) Run the watchman loop body. Its first tick reads
    //     `resident-daemons.json` and ensures a daemon for the project.
    let watch = spawn_boards_watch(&bin, &home);
    kills.add(watch.pid());

    let healed = wait_until(HEAL_TIMEOUT, || {
        yi_agent_boards::board_daemon::is_running(&project)
            && daemon_pids(&project).first().copied() != Some(daemon)
    });
    assert!(
        healed,
        "the watchman must bring the killed daemon back within {HEAL_TIMEOUT:?}"
    );

    // The healed daemon is a fresh process, and the old one is really gone.
    let healed_pid = daemon_pids(&project)[0];
    assert_ne!(healed_pid, daemon, "the heal must be a new daemon process");
    assert!(!pid_alive(daemon), "the killed daemon must not be alive");

    // Acceptance §5.3: never two daemons for one project. The kernel lock is the
    // arbiter, so exactly one process holds it.
    assert_eq!(
        daemon_pids(&project).len(),
        1,
        "exactly one daemon may hold the project's runtime lock"
    );

    // Idempotence must be observed, not assumed: no fixed sleep. Kill the healed
    // daemon and poll until the watchman has demonstrably ticked again (it brings
    // up a *third* daemon). That tick is the proof the heal is idempotent across
    // ticks — the daemon came back and exactly one process holds the lock — without
    // depending on the loop having completed a tick after 1.5s.
    unsafe {
        libc::kill(healed_pid, libc::SIGKILL);
    }
    let ticked = wait_until(HEAL_TIMEOUT, || {
        let held = daemon_pids(&project);
        held.len() == 1 && held[0] != healed_pid
    });
    assert!(
        ticked,
        "the watchman must have run another tick and healed {healed_pid} within {HEAL_TIMEOUT:?}"
    );
    assert!(
        yi_agent_boards::board_daemon::is_running(&project),
        "the daemon must be answering after the second heal"
    );
    assert_eq!(
        daemon_pids(&project).len(),
        1,
        "watchman ticks must not accumulate daemons"
    );

    // The daemon the watchman launched is supervised by the daemon itself, so a
    // plugin holds the queue's advance right. The daemon was started detached
    // (not our child), so it cannot be reaped — stop it, then sweep every
    // remaining project process (including a plugin orphaned by the SIGKILL
    // above), so no fixture outlives the test. The sweep is then verified: a leak
    // fails the test rather than being quietly tolerated.
    let _ = yi_agent_boards::board_daemon::stop(&project);
    kill_project_processes(&project);
    verify_no_project_processes(&project);
}

// ---------------------------------------------------------------------------
// Test 2 — an orphaned plugin exits after its daemon dies.
// ---------------------------------------------------------------------------

/// Acceptance §5.4: the single-instance lock admits only one advancing plugin,
/// and an orphan whose daemon is gone yields the right instead of holding it
/// forever.
///
/// Faithfulness note: this test drives a real daemon and the real plugin, but
/// **not** real launchd. The daemon starts the plugin from a project supervisor
/// manifest (byte-for-byte the shape `yi-agent-boards::scaffold` installs, only
/// the interval is 1s instead of 10s and the command points at the worktree
/// build). The installed Homebrew plugin on this machine predates Task 8, so
/// running it would observe the *old* behaviour; the worktree build is the
/// artifact under test.
///
/// The one kqueue-grade property that is out of scope — instant (`SIGKILL` →
/// immediate) orphan detection — is a documented non-goal (§4.6, §6). The
/// observable invariant is the weaker, sufficient one: the orphan exits within
/// a bounded number of probe intervals, releasing the lock so the next plugin
/// can advance.
///
/// Scope of the "single instance" observation: the second `plugin run` is seen to
/// stay *alive* yet never appear as a lock holder. That is a liveness / identity
/// check on real processes — it is **not** an in-process proof that `flock`
/// refuses a second holder (that proof is the plugin's own
/// `single_instance::tests::a_second_instance_cannot_take_the_lock`). Likewise
/// this test never starts a card, so "no card is launched twice" is inferred from
/// the single-advancer lock plus the plugin's queued-launch unit tests, not
/// observed here.
#[test]
fn an_orphan_plugin_exits_after_the_daemon_dies() {
    if let Some(reason) = gate_reason() {
        eprintln!("skipping orphan-plugin end-to-end: {reason}");
        return;
    }
    // No Homebrew requirement here: this test drives the worktree-built plugin,
    // so it must run on a machine without the published binary installed.
    let plugin = worktree_plugin_bin();
    if !plugin.is_file() {
        eprintln!(
            "skipping orphan-plugin end-to-end: {} is not built (run `cargo build -p \
             superpowers-kanban-runner --bin superpowers-kanban` in plugins/superpowers-kanban)",
            plugin.display()
        );
        return;
    }

    let root = tempfile::TempDir::new().unwrap();
    let home = root.path().join("home");
    let project = root.path().join("project");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    let project = project.canonicalize().unwrap();
    scaffold_fast_project(&project, &plugin);

    let bin = yi_agent_bin();
    let mut kills = KillOnDrop::default();
    let _sweeper = ProjectSweeper(project.clone());

    // (1) A real daemon, which starts the plugin from the manifest.
    let mut daemon = spawn_daemon(&bin, &project, &home);
    kills.add(daemon.pid());
    let daemon_pid = wait_for_daemon(&project, Duration::from_secs(10));
    let first_plugin = wait_for_plugin(&project, Duration::from_secs(10));

    // (2) Single-instance: a second `plugin run` on the same state directory
    //     cannot take the lock (the plugin's own unit test proves the `flock`
    //     refusal; here we observe the consequence). It waits (the production
    //     contract), so it stays alive while the first holder keeps the lock.
    let mut second = Command::new(&plugin);
    second
        .args([
            "run",
            "--runtime-dir",
            &runtime_dir(&project).to_string_lossy(),
            "--state-dir",
            &state_dir(&project).to_string_lossy(),
            "--project-root",
            &project.to_string_lossy(),
            "--interval-secs",
            "1",
        ])
        .env("HOME", &home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let second_pid = second.spawn().expect("spawn the second plugin").id() as i32;

    // No fixed settle: poll observable state. The second instance is alive until
    // the very end, so a bounded wait that keeps seeing it alive and out of the
    // lock-holder set is exactly the invariant (a live process the kernel refused
    // the lock), not a guess about how long the attempt takes.
    let stable = wait_until(Duration::from_secs(2), || {
        pid_alive(second_pid) && plugin_pids(&project) == vec![first_plugin]
    });
    assert!(
        stable,
        "the second plugin must stay alive and out of the lock while the first holds it"
    );
    assert!(
        pid_alive(second_pid),
        "a second plugin must wait for the lock, not exit"
    );
    assert_eq!(
        plugin_pids(&project),
        vec![first_plugin],
        "the second plugin must not acquire the lock while the first holds it"
    );
    unsafe {
        libc::kill(second_pid, libc::SIGKILL);
    }

    // (3) Kill the daemon with SIGKILL. Its plugin is NOT in the same process
    //     group (`Command::spawn` without setsid), so the plugin is left as an
    //     orphan — exactly the failure §4.6 describes.
    unsafe {
        libc::kill(daemon_pid, libc::SIGKILL);
    }
    // Reap the daemon here. `ChildGuard::kill` does `wait()`, so the outer
    // `kills` guard cannot leave it a zombie the way a bare `SIGKILL` would: a
    // zombie is reported alive by `kill(pid, 0)`.
    daemon.kill();
    assert!(
        wait_until(Duration::from_secs(10), || !pid_alive(daemon_pid)),
        "the killed daemon must disappear"
    );

    // (4) The orphan must notice the dead daemon and exit, releasing the lock.
    let yielded = wait_until(ORPHAN_EXIT_TIMEOUT, || {
        plugin_pids(&project).is_empty() && !pid_alive(first_plugin)
    });
    assert!(
        yielded,
        "the orphaned plugin must exit within {ORPHAN_EXIT_TIMEOUT:?} and release the lock"
    );

    // (5) A fresh daemon gets a fresh plugin that takes the lock immediately,
    //     and there is still exactly one advancing plugin — the orphan did not
    //     linger to double-advance the queue.
    let mut daemon2 = spawn_daemon(&bin, &project, &home);
    kills.add(daemon2.pid());
    let _daemon2_pid = wait_for_daemon(&project, Duration::from_secs(10));
    let second_plugin = wait_for_plugin(&project, Duration::from_secs(15));
    assert_ne!(
        second_plugin, first_plugin,
        "the replacement must be a new plugin process"
    );
    assert_eq!(
        plugin_pids(&project),
        vec![second_plugin],
        "exactly one plugin may hold the advance right after recovery"
    );

    // Cleanup: stop the daemon; its plugin becomes an orphan and will exit on
    // its own, but sweep it explicitly so nothing outlives the test, then verify
    // the sweep really drained the locks (a leak fails the test).
    let _ = yi_agent_boards::board_daemon::stop(&project);
    daemon2.kill();
    kill_project_processes(&project);
    verify_no_project_processes(&project);
}
