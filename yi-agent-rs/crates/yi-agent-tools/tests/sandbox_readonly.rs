//! The read-only sandbox must still let well-behaved tooling run.
//!
//! `sandbox-exec` denies `file-write*` wholesale in read-only mode, but Git
//! opens `/dev/null` for reading *and* writing on every invocation, including
//! purely read-only ones. Without an explicit allowance the command dies with
//! `could not open '/dev/null' for reading and writing: Operation not
//! permitted`, so a read-only child could not inspect the repository at all
//! and every `git` call became a hard failure.
//!
//! The allowance must not weaken the mode: writing into the workspace still has
//! to be denied, which the second test pins.

#![cfg(target_os = "macos")]

use std::path::Path;
use std::process::Command;

use yi_agent_tools::{SandboxMode, SandboxPolicy};

fn sandbox_exec_available() -> bool {
    Path::new("/usr/bin/sandbox-exec").is_file()
}

fn run_sandboxed(policy: &SandboxPolicy, shell_command: &str, cwd: &Path) -> std::process::Output {
    let (program, args) = policy
        .command(shell_command, cwd)
        .expect("the read-only policy should build a command");
    Command::new(&program)
        .args(&args)
        .current_dir(cwd)
        .output()
        .expect("should spawn the sandboxed command")
}

#[test]
fn read_only_sandbox_can_run_git() {
    if !sandbox_exec_available() {
        eprintln!("skip: /usr/bin/sandbox-exec is unavailable");
        return;
    }
    // The crate directory is inside this repository, so `git` has a repo to
    // read. Nothing here writes, so the read-only mode is the correct mode.
    let cwd = std::env::current_dir().expect("a current directory");
    let policy = SandboxPolicy::new(SandboxMode::ReadOnly, &cwd, Vec::new());

    let output = run_sandboxed(&policy, "git log --oneline -1", &cwd);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "git must run under the read-only sandbox; exit={:?} stderr={stderr}",
        output.status.code()
    );
    assert!(
        !stderr.contains("Operation not permitted"),
        "the read-only sandbox denied something git needs: {stderr}"
    );
}

#[test]
fn read_only_sandbox_still_denies_workspace_writes() {
    if !sandbox_exec_available() {
        eprintln!("skip: /usr/bin/sandbox-exec is unavailable");
        return;
    }
    let workspace = tempfile::TempDir::new().unwrap();
    let cwd = workspace.path();
    let policy = SandboxPolicy::new(SandboxMode::ReadOnly, cwd, Vec::new());

    let output = run_sandboxed(&policy, "echo leaked > probe.txt", cwd);
    assert!(
        !output.status.success(),
        "read-only must not permit writing into the workspace"
    );
    assert!(
        !cwd.join("probe.txt").exists(),
        "read-only must not leave a written file behind"
    );
}
