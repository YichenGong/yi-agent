//! The private managed roles refuse to run unless a launcher authorized them.
//!
//! These run the real binary, because the claim being tested is about a *process*:
//! "this child cannot have been started by hand". A unit test that called the
//! function would not exercise the environment scrubbing or the CLI wiring.

use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

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
