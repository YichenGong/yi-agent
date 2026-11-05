//! The private managed roles refuse to run unless a launcher authorized them.
//!
//! These run the real binary, because the claim being tested is about a *process*:
//! "this child cannot have been started by hand". A unit test that called the
//! function would not exercise the environment scrubbing or the CLI wiring.

use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

type Output = std::process::Output;

fn binary() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_yi-agent"))
}

fn run(args: &[&str], env: &[(&str, &str)]) -> std::process::Output {
    let mut command = Command::new(binary());
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Never inherit a real launcher's authorization from the test runner.
        .env_remove("YI_AGENT_BOOTSTRAP_FD")
        .env_remove("YI_AGENT_SCOPE_KEY");
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().unwrap()
}

/// A hand-started guardian is refused, and says why.
#[test]
fn the_guardian_role_refuses_to_run_by_hand() {
    let output = run(&["internal-runtime", "guardian"], &[]);
    assert!(
        !output.status.success(),
        "a hand-started guardian must not run"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("YI_AGENT_BOOTSTRAP_FD"), "{stderr}");
    assert!(stderr.contains("never by hand"), "{stderr}");
}

/// A hand-started managed daemon is refused. The public `daemon serve` keeps its
/// old unmanaged semantics; these are different commands on purpose.
#[test]
fn the_managed_daemon_role_refuses_to_run_by_hand() {
    let output = run(&["internal-runtime", "daemon"], &[]);
    assert!(
        !output.status.success(),
        "a hand-started managed daemon must not run"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("YI_AGENT_BOOTSTRAP_FD"), "{stderr}");
}

/// A standard stream is not an authorization: `YI_AGENT_BOOTSTRAP_FD=1` must be
/// rejected rather than read as protocol.
#[test]
fn a_standard_stream_is_not_accepted_as_authorization() {
    let output = run(
        &["internal-runtime", "guardian"],
        &[("YI_AGENT_BOOTSTRAP_FD", "1")],
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("standard stream"), "{stderr}");
}

/// With a real open descriptor but no scope key, the role still refuses: a managed
/// role is told which runtime it serves, it never guesses one.
///
/// The descriptor is duplicated onto a fixed number in the child so this proves the
/// *next* check, not the "descriptor is not open" check.
#[test]
fn a_role_without_a_scope_key_still_refuses() {
    let filler = std::fs::File::open("/dev/null").unwrap();
    let mut command = Command::new(binary());
    command
        .args(["internal-runtime", "guardian"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove("YI_AGENT_SCOPE_KEY")
        .env("YI_AGENT_BOOTSTRAP_FD", "9");
    let raw = filler.as_raw_fd();
    // SAFETY: `dup2` in the forked child only rearranges this process's descriptor
    // table before `exec`; nothing else runs concurrently there.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(raw, 9) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let output = command.output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("YI_AGENT_SCOPE_KEY"), "{stderr}");
}

/// A launched managed role whose bootstrap peer answers EOF before greeting it
/// must exit nonzero, and must never have bound the business socket.
///
/// Silence is not "unmanaged" for the managed role: a launcher that died between
/// the fork and the handshake proves nothing about who authorized this process.
#[test]
fn a_bootstrap_eof_refuses_the_managed_daemon_before_it_serves() {
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path().join("project");
    let runtime_dir = dir.path().join("runtime");
    std::fs::create_dir_all(&workdir).unwrap();
    std::fs::create_dir_all(&runtime_dir).unwrap();

    let (parent_end, child_end) = std::os::unix::net::UnixStream::pair().unwrap();
    let child_fd = child_end.as_raw_fd();
    let mut command = Command::new(binary());
    command
        .args(["internal-runtime", "daemon"])
        .current_dir(&workdir)
        .env_remove("YI_AGENT_AUTHORITY_BRIDGE_FD")
        .env(
            "YI_AGENT_SCOPE_KEY",
            runtime_dir.to_string_lossy().to_string(),
        )
        .env(
            "YI_AGENT_INSTANCE_HOME",
            dir.path().join("home").to_string_lossy().to_string(),
        )
        .env(
            "YI_AGENT_RUNTIME_DIR",
            runtime_dir.to_string_lossy().to_string(),
        )
        .env("YI_AGENT_BOOTSTRAP_FD", "9")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: `pre_exec` only rearranges this fork's descriptor table before `exec`.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(child_fd, 9) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().unwrap();
    // The signal: the launcher's end closes without ever writing a hello.
    drop(parent_end);
    drop(child_end);

    let output = wait_bounded(child, std::time::Duration::from_secs(30));
    assert!(
        !output.status.success(),
        "a managed daemon without a hello must not run: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("scope") || stderr.contains("handshake") || stderr.contains("guardian"),
        "the refusal must say why: {stderr}"
    );
    assert!(
        !runtime_dir.join("runtime.sock").exists(),
        "a refused managed daemon must not leave a business socket behind"
    );
}

/// A launcher that opens the channel and then says nothing past the read budget
/// must produce the same refusal: a silent peer is not an authorization.
#[test]
fn a_silent_bootstrap_peer_refuses_the_managed_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path().join("project");
    let runtime_dir = dir.path().join("runtime");
    std::fs::create_dir_all(&workdir).unwrap();
    std::fs::create_dir_all(&runtime_dir).unwrap();

    let (parent_end, child_end) = std::os::unix::net::UnixStream::pair().unwrap();
    let child_fd = child_end.as_raw_fd();
    let mut command = Command::new(binary());
    command
        .args(["internal-runtime", "daemon"])
        .current_dir(&workdir)
        .env_remove("YI_AGENT_AUTHORITY_BRIDGE_FD")
        .env(
            "YI_AGENT_SCOPE_KEY",
            runtime_dir.to_string_lossy().to_string(),
        )
        .env(
            "YI_AGENT_INSTANCE_HOME",
            dir.path().join("home").to_string_lossy().to_string(),
        )
        .env(
            "YI_AGENT_RUNTIME_DIR",
            runtime_dir.to_string_lossy().to_string(),
        )
        .env("YI_AGENT_BOOTSTRAP_FD", "9")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: as above.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(child_fd, 9) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().unwrap();
    drop(child_end);
    // Hold the peer open but silent, past the lifeline read budget.
    let output = wait_bounded(child, std::time::Duration::from_secs(40));
    drop(parent_end);
    assert!(!output.status.success(), "{output:?}");
    assert!(
        !runtime_dir.join("runtime.sock").exists(),
        "a silent launcher must not leave a business socket behind"
    );
}

/// Runs `child` to completion, killing it if it outlives `budget`.
fn wait_bounded(mut child: std::process::Child, budget: std::time::Duration) -> Output {
    let deadline = std::time::Instant::now() + budget;
    loop {
        match child.try_wait().unwrap() {
            Some(_) => return child.wait_with_output().unwrap(),
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                return child.wait_with_output().unwrap();
            }
            None => std::thread::sleep(std::time::Duration::from_millis(25)),
        }
    }
}

/// The managed roles are hidden from the command list: a user must not be led to
/// type them, and their names are not a stable public surface.
#[test]
fn the_managed_roles_are_not_advertised() {
    let output = run(&["internal-runtime", "--help"], &[]);
    let text = String::from_utf8_lossy(&output.stdout);
    let commands = text
        .split("Commands:")
        .nth(1)
        .expect("the help must have a command list");
    assert!(
        !commands.contains("guardian") && !commands.contains("daemon"),
        "the managed roles must stay out of the command list:\n{commands}"
    );
    // The prose may still mention them; what must not happen is a suggested command.
    assert!(commands.contains("identity"), "{commands}");
}
