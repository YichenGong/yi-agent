//! Real child processes for the process-group tests, plus the helper they run.
//!
//! The helper is *this integration-test binary* re-executed as a single ignored
//! test. Choosing that over guessing another target's hashed path keeps the tests
//! working when the build layout changes, and avoids scanning `target/`.
//!
//! The helper is inert without the guardian's ready descriptor. Running
//! `cargo test -- --ignored` by hand therefore cannot start stray processes: it
//! exits with an explicit diagnostic instead.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use yi_agent_supervisors::lifecycle::guardian::{
    ManagedCommand, ProcessOwner, READY_CONSUMED_ENV, READY_FD, READY_FD_ENV, TaskExecutionKey,
};

/// Local alias so the fixture reads naturally at the call site.
const YI_AGENT_READY_CONSUMED: &str = READY_CONSUMED_ENV;

/// The ignored test that acts as the helper.
pub const HELPER_TEST: &str = "process_fixture_helper";

/// How long a fixture waits for a marker before failing the test.
pub const MARKER_TIMEOUT: Duration = Duration::from_secs(10);

/// Removes the temporary markers a test created, even when an assertion fails.
pub struct MarkerGuard {
    markers: Vec<PathBuf>,
}

impl MarkerGuard {
    pub fn new() -> Self {
        Self {
            markers: Vec::new(),
        }
    }

    pub fn path(&mut self, label: &str) -> PathBuf {
        // A counter plus a nanosecond stamp: two markers created in the same
        // microsecond by the same test still differ.
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let marker = std::env::temp_dir().join(format!(
            "yi-agent-fixture-{}-{label}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&marker);
        self.markers.push(marker.clone());
        marker
    }
}

impl Default for MarkerGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for MarkerGuard {
    fn drop(&mut self) {
        for marker in &self.markers {
            let _ = std::fs::remove_file(marker);
        }
    }
}

/// A command that re-executes this test binary as the fixture helper.
pub fn helper(
    owner: ProcessOwner,
    role_args: &[&str],
    execution: Option<TaskExecutionKey>,
) -> ManagedCommand {
    let mut args = vec![
        "--exact".to_string(),
        HELPER_TEST.to_string(),
        "--ignored".to_string(),
        "--nocapture".to_string(),
    ];
    args.extend(role_args.iter().map(|arg| arg.to_string()));
    ManagedCommand {
        executable: std::env::current_exe().expect("the test binary path is known"),
        args,
        env: Vec::new(),
        cwd: std::env::temp_dir(),
        owner,
        root: None,
        execution,
    }
}

/// A helper that forks a grandchild, then exits, leaving the grandchild alive.
///
/// This is the case a "kill the direct child and wait for it" implementation gets
/// wrong: the parent is reaped while the group still runs.
pub fn parent_exits_leaving_child(owner: ProcessOwner, marker: &Path) -> ManagedCommand {
    helper(owner, &["parent-exits", &marker.to_string_lossy()], None)
}

/// A helper that stays alive appending to `marker` every 20ms.
pub fn steady_writer(
    owner: ProcessOwner,
    marker: &Path,
    execution: Option<TaskExecutionKey>,
) -> ManagedCommand {
    helper(owner, &["write", &marker.to_string_lossy()], execution)
}

/// A helper that records which descriptors it inherited, then sleeps.
pub fn descriptor_reader(owner: ProcessOwner, marker: &Path) -> ManagedCommand {
    helper(owner, &["list-fds", &marker.to_string_lossy()], None)
}

/// A helper that exits immediately with status 0.
pub fn short_lived(owner: ProcessOwner) -> ManagedCommand {
    helper(owner, &["exit"], None)
}

/// Blocks until `marker` exists and is non-empty, or the timeout elapses.
pub fn wait_for_marker(marker: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if std::fs::metadata(marker)
            .map(|meta| meta.len() > 0)
            .unwrap_or(false)
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

/// Reads `marker` as a string, waiting for it to appear first.
pub fn read_marker(marker: &Path) -> String {
    assert!(
        wait_for_marker(marker, MARKER_TIMEOUT),
        "marker {} never appeared",
        marker.display()
    );
    std::fs::read_to_string(marker).unwrap_or_default()
}

/// The size of `marker`, or 0 when it does not exist.
pub fn marker_len(marker: &Path) -> u64 {
    std::fs::metadata(marker)
        .map(|meta| meta.len())
        .unwrap_or(0)
}

/// Whether `pid` is currently a member of `pgid`.
pub fn pid_in_group(pid: i32, pgid: i32) -> bool {
    unsafe { libc::getpgid(pid) == pgid }
}

/// Starts a detached `sleep`, returning its PID (which is also its PGID).
///
/// Used to place an unmanaged-looking group in the process table so a journal can
/// be pointed at a group the guardian never started.
pub fn detached_sleeper() -> std::process::Child {
    use std::os::unix::process::CommandExt;
    let mut command = std::process::Command::new("/bin/sleep");
    command
        .arg("60")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn().expect("start a detached sleeper")
}

/// The helper entry point; returns only when this process is *not* the helper.
///
/// Called from the `process_fixture_helper` test at the top of the test binary's
/// run, so a normal `cargo test` is unaffected.
pub fn run_helper_if_requested() {
    let args: Vec<String> = std::env::args().collect();
    let Some(position) = args.iter().position(|arg| arg == HELPER_TEST) else {
        return;
    };
    // libtest keeps its own flags after the test name; the role is the first
    // non-flag token, so `--ignored --nocapture` never becomes a role.
    let role_args: Vec<String> = args[position + 1..]
        .iter()
        .filter(|arg| !arg.starts_with('-'))
        .cloned()
        .collect();
    let role_args: Vec<&str> = role_args.iter().map(String::as_str).collect();

    // A launcher in front of this program may have consumed the barrier already;
    // reading it again would block forever on a byte that is gone.
    let already_released = std::env::var(YI_AGENT_READY_CONSUMED).is_ok();

    // Without the guardian's descriptor there is no authority to run anything.
    let ready_fd: i32 = std::env::var(READY_FD_ENV)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(READY_FD);
    if std::env::var(READY_FD_ENV).is_err() {
        eprintln!("process_fixture_helper: no ready descriptor; refusing to start");
        std::process::exit(2);
    }
    if !already_released && read_one_byte(ready_fd).is_err() {
        // EOF without a byte means the guardian died before registration became
        // durable. Exiting here, having run nothing, is the whole point.
        eprintln!("process_fixture_helper: ready barrier closed before start");
        std::process::exit(3);
    }

    let role = role_args.first().copied().unwrap_or("");
    match role {
        "parent-exits" => {
            let marker = PathBuf::from(&role_args[1]);
            // The grandchild inherits this group, so it outlives the parent while
            // remaining reachable by a group kill.
            let grandchild = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(format!(
                    "printf grandchild > {} && exec sleep 120",
                    marker.display()
                ))
                .spawn()
                .expect("fork a grandchild");
            eprintln!("helper: grandchild pid {}", grandchild.id());
            // Deliberately *not* waiting: the parent must be reaped while the
            // group is still alive.
            std::process::exit(0);
        }
        "write" => {
            let marker = PathBuf::from(&role_args[1]);
            loop {
                use std::io::Write;
                if let Ok(mut file) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&marker)
                {
                    let _ = file.write_all(b"tick\n");
                    let _ = file.flush();
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        "list-fds" => {
            let marker = PathBuf::from(&role_args[1]);
            let mut inherited = Vec::new();
            for fd in 0..64 {
                if unsafe { libc::fcntl(fd, libc::F_GETFD) } != -1 {
                    inherited.push(fd);
                }
            }
            let list = inherited
                .iter()
                .map(|fd| fd.to_string())
                .collect::<Vec<_>>()
                .join(",");
            std::fs::write(&marker, list).expect("record inherited descriptors");
            std::thread::sleep(Duration::from_secs(60));
            std::process::exit(0);
        }
        "streams" => {
            // Writes one line to each stream, then exits: proves the channel put
            // the caller's two pipes on the right descriptors.
            use std::io::Write;
            let mut out = std::io::stdout();
            let mut err = std::io::stderr();
            let _ = out.write_all(b"on-stdout\n");
            let _ = out.flush();
            let _ = err.write_all(b"on-stderr\n");
            let _ = err.flush();
            std::process::exit(0);
        }
        "streams-then-hang" => {
            // Writes both lines, then stays alive so the group outlives its leader
            // only if the caller terminates it.
            use std::io::Write;
            let mut out = std::io::stdout();
            let mut err = std::io::stderr();
            let _ = out.write_all(b"on-stdout\n");
            let _ = out.flush();
            let _ = err.write_all(b"on-stderr\n");
            let _ = err.flush();
            // A grandchild in the same group, so "did the whole group stop"
            // is not answered by the leader alone.
            let grandchild = std::process::Command::new("/bin/sleep")
                .arg("120")
                .spawn()
                .expect("fork a hanging grandchild");
            eprintln!("helper: hanging grandchild {}", grandchild.id());
            std::thread::sleep(Duration::from_secs(120));
            std::process::exit(0);
        }
        "stdin-echo" => {
            // Copies stdin to stdout until EOF. A merged or backwards stdio would
            // produce nothing, or would block forever.
            use std::io::{Read, Write};
            let mut input = Vec::new();
            let _ = std::io::stdin().read_to_end(&mut input);
            let mut out = std::io::stdout();
            let _ = out.write_all(&input);
            let _ = out.flush();
            std::process::exit(0);
        }
        "exit" => std::process::exit(0),
        other => {
            eprintln!("process_fixture_helper: unknown role {other:?}");
            std::process::exit(2);
        }
    }
}

fn read_one_byte(fd: i32) -> std::io::Result<()> {
    let mut byte = [0u8; 1];
    let read = unsafe { libc::read(fd, byte.as_mut_ptr() as *mut libc::c_void, 1) };
    if read == 1 {
        Ok(())
    } else if read == 0 {
        Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "ready barrier closed",
        ))
    } else {
        Err(std::io::Error::last_os_error())
    }
}
