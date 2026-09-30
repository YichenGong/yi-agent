//! Regression: interrupting `yi-agent run` (headless) must reap the tool's
//! process group, not just kill the CLI process.
//!
//! Before the fix, `SIGINT`/`SIGTERM` hit the default disposition: the process
//! died before the in-flight bash tool's future was dropped, so the bash tool's
//! `ProcessGroupGuard::drop` / `kill_on_drop` never ran and the command's whole
//! process group was reparented to init and kept running.
//!
//! This test drives the real binary through a mock OpenAI-compatible gateway
//! that scripts a single `bash` call running `sleep 60`. It then signals the
//! headless process and asserts the `sleep`'s process group is gone.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn yi_agent_bin() -> PathBuf {
    option_env!("CARGO_BIN_EXE_yi-agent")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/debug/yi-agent"))
}

/// Serve one OpenAI-compatible SSE completion per request: the first emits a
/// `bash` tool call, every later one (a tool result is in the transcript)
/// finishes the turn.
fn spawn_mock_gateway(port: u16, probe_cmd: &str) -> std::thread::JoinHandle<()> {
    let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind mock gateway");
    let cmd = probe_cmd.to_string();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut buf = vec![0u8; 65536];
            let n = stream.read(&mut buf).unwrap_or(0);
            let body = String::from_utf8_lossy(&buf[..n]).to_string();
            let saw_tool_result =
                body.contains("\"role\":\"tool\"") || body.contains("\"role\": \"tool\"");
            let frames = if saw_tool_result {
                vec![
                    r#"{"choices":[{"delta":{"content":"done"}}]}"#.to_string(),
                    r#"{"choices":[{"finish_reason":"stop","delta":{}}]}"#.to_string(),
                ]
            } else {
                let args = serde_json::json!({ "command": cmd, "timeout": 600 }).to_string();
                vec![
                    format!(
                        r#"{{"choices":[{{"delta":{{"tool_calls":[{{"index":0,"id":"c1","function":{{"name":"bash","arguments":""}}}}]}}}}]}}"#
                    ),
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
            for f in frames {
                out.push_str(&format!("data: {f}\n\n"));
            }
            out.push_str("data: [DONE]\n\n");
            let _ = stream.write_all(out.as_bytes());
            let _ = stream.flush();
        }
    })
}

/// Owns the headless child so it is killed and waited on even when an assertion
/// unwinds (otherwise a failing test leaks a live `yi-agent run`).
struct ChildGuard(std::process::Child);

impl ChildGuard {
    fn reap(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.reap();
    }
}

fn live_group_members(pgid: i32) -> Vec<i32> {
    let out = Command::new("ps")
        .args(["-o", "pid=,stat=,pgid=", "-ax"])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let pid: i32 = it.next()?.parse().ok()?;
            let stat = it.next()?;
            let pg: i32 = it.next()?.parse().ok()?;
            (pg == pgid && !stat.starts_with('Z')).then_some(pid)
        })
        .collect()
}

/// Launch headless against the mock gateway and wait until the bash tool is up.
/// Returns the child and the tool's process-group id.
fn start_run_with_live_tool(port: u16, pid_file: &str) -> (ChildGuard, i32) {
    let workdir = tempfile::TempDir::new().expect("tempdir");
    let workdir_path = workdir.path().to_path_buf();
    // Keep the tempdir alive for the duration of the test process.
    std::mem::forget(workdir);
    let _ = std::fs::remove_file(pid_file);

    let child = Command::new(yi_agent_bin())
        .arg("--workdir")
        .arg(&workdir_path)
        .args(["--provider", "openai"])
        .arg("--api-url")
        .arg(format!("http://127.0.0.1:{port}"))
        .args(["--api-key", "test-key", "--model", "gpt-4o", "--yolo"])
        .args(["run", "--json", "go"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn yi-agent run");
    // From here on the child is owned by a guard that kills+waits on every
    // path, including the timeout panic below.
    let child = ChildGuard(child);

    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if let Ok(raw) = std::fs::read_to_string(pid_file) {
            if let Ok(pgid) = raw.trim().parse::<i32>() {
                return (child, pgid);
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("the bash tool never started (no pid file)");
}

fn signal_and_expect_group_reaped(signal: i32) {
    let port = 9000 + (signal as u16 % 500);
    // One pid file per signal so the two tests can run in parallel.
    let pid_file = format!("/tmp/yi-headless-signal-tool-{signal}.pid");
    let cmd = format!("echo $$ > {pid_file}; sleep 60");
    let _gateway = spawn_mock_gateway(port, &cmd);

    let (mut child, pgid) = start_run_with_live_tool(port, &pid_file);
    assert!(
        !live_group_members(pgid).is_empty(),
        "precondition: the probe command must actually be running"
    );

    unsafe { libc::kill(child.0.id() as i32, signal) };

    // The graceful path cancels the run; the run loop then drops the tool
    // future, which reaps the group. Give it a generous window.
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if live_group_members(pgid).is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let survivors = live_group_members(pgid);
    child.reap();
    for pid in &survivors {
        unsafe { libc::kill(*pid, libc::SIGKILL) };
    }
    assert!(
        survivors.is_empty(),
        "signal {signal} left the bash tool's process group (pgid {pgid}) alive: {survivors:?}"
    );
}

#[test]
fn sigint_reaps_the_running_bash_tool_group() {
    signal_and_expect_group_reaped(libc::SIGINT);
}

#[test]
fn sigterm_reaps_the_running_bash_tool_group() {
    signal_and_expect_group_reaped(libc::SIGTERM);
}
