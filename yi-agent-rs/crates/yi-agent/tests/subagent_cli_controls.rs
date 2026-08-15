use std::process::Command;

use tempfile::TempDir;
use yi_agent_store::ipc::{Daemon, IpcRequest, IpcResponse, send_request};

fn yi_agent_bin() -> std::path::PathBuf {
    std::env::var_os("CARGO_BIN_EXE_yi-agent")
        .map(Into::into)
        .expect("Cargo must provide the yi-agent binary")
}

fn invoke(runtime: &std::path::Path, arguments: &[&str]) -> std::process::Output {
    Command::new(yi_agent_bin())
        .args(arguments)
        .env("YI_AGENT_RUNTIME_DIR", runtime)
        .output()
        .expect("run yi-agent control command")
}

#[test]
fn run_help_advertises_explicit_subagent_opt_in() {
    let output = Command::new(yi_agent_bin())
        .args(["run", "--help"])
        .output()
        .expect("run yi-agent help");
    assert!(output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("--subagents"),
        "run help must document the delegation opt-in"
    );
}

#[test]
fn cli_task_observation_controls_render_daemon_owned_task_identity_without_secrets() {
    let directory = TempDir::new().unwrap();
    let runtime = directory.path().join("runtime");
    let daemon = Daemon::start(&runtime, directory.path().join("runtime.sqlite")).unwrap();
    let IpcResponse::SessionCreated { root_task_id, .. } =
        send_request(daemon.socket_path(), IpcRequest::CreateSession).unwrap()
    else {
        panic!("expected runtime session");
    };

    for (arguments, renders_task_id) in [
        (vec!["agents", "--all"], true),
        (vec!["agent", "show", &root_task_id], true),
        // A new session need not have emitted events, but an empty event stream
        // remains a successful daemon-backed observation operation.
        (vec!["agent", "events", &root_task_id], false),
    ] {
        let output = invoke(&runtime, &arguments);
        assert!(
            output.status.success(),
            "control command {arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        if renders_task_id {
            assert!(stdout.contains(&root_task_id), "missing task ID: {stdout}");
        }
        assert!(!stdout.contains("cli-control-secret-sentinel"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("cli-control-secret-sentinel"));
    }
}

#[test]
fn cli_confirmation_controls_reject_yes_without_a_preview_token() {
    let directory = TempDir::new().unwrap();
    let runtime = directory.path().join("runtime");
    let daemon = Daemon::start(&runtime, directory.path().join("runtime.sqlite")).unwrap();
    let IpcResponse::SessionCreated { root_task_id, .. } =
        send_request(daemon.socket_path(), IpcRequest::CreateSession).unwrap()
    else {
        panic!("expected runtime session");
    };

    for arguments in [
        vec!["agent", "cancel", &root_task_id, "--yes"],
        vec!["agent", "accept", &root_task_id, "--yes"],
        vec!["agent", "rework", &root_task_id, "feedback", "--yes"],
        vec!["agent", "reject", &root_task_id, "reason", "--yes"],
    ] {
        let output = invoke(&runtime, &arguments);
        assert!(
            !output.status.success(),
            "{arguments:?} must require preview token"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("confirmation token is required"),
            "unexpected confirmation error: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
