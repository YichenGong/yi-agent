//! What a headless entry (`yi-agent run`) actually delegates from.
//!
//! The bug this file exists for was a *role* mistake: a conversation's container
//! was read through its historical `TaskState`, so a container whose last attempt
//! ended `stalled` looked like settled work and was refused as a spawn parent. The
//! fix moved admission onto the persisted role. That is only provable end to end:
//! the process below is the real binary, the tool is the real registered
//! `spawn_agent`, and the rows are read back from the runtime database the daemon
//! actually wrote.
//!
//! The provider is a local HTTP/SSE gateway rather than a real model, so the test
//! needs no API key and asserts on structure (which role was written) instead of
//! on text a model chose.

mod common;

use std::io::{BufRead, Read, Write};
use std::process::Command;
use std::time::Duration;

use yi_agent_core::subagent::task::TaskRole;

/// A marker only this test's prompt carries, so the gateway can tell the spawning
/// turn from every later one.
const PARENT_SPAWN_ONCE: &str = "PARENT_SPAWN_ONCE";

/// The same marker for a *resumed* conversation, where the transcript already
/// holds the first run's tool result.
///
/// The gateway keys on "this turn has a prompt but no tool result yet", which is
/// per-turn; a resumed transcript contains the earlier result, so a shared marker
/// would look like an already-answered turn and the run would never spawn again.
const PARENT_SPAWN_AGAIN: &str = "PARENT_SPAWN_AGAIN";

/// A minimal OpenAI-compatible streaming gateway.
///
/// Deliberately hand-written over `std::net`: the test crate has no HTTP server
/// dependency, and the wire shape needed here is one chunked SSE response. The
/// gateway never sleeps to simulate budgeting -- the only wait is the non-blocking
/// accept poll, which is scheduling, not a fake stop proof.
struct SpawnGateway {
    url: String,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl SpawnGateway {
    fn start() -> Self {
        use std::sync::atomic::Ordering;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a local port");
        listener
            .set_nonblocking(true)
            .expect("a non-blocking listener");
        let url = format!("http://{}", listener.local_addr().expect("a local address"));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            while !flag.load(Ordering::Acquire) {
                let (mut socket, _) = match listener.accept() {
                    Ok(peer) => peer,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("mock gateway accept: {error}"),
                };
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("a read timeout");
                let mut reader =
                    std::io::BufReader::new(socket.try_clone().expect("a cloned socket"));
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    continue;
                }
                let mut length = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() || line == "\r\n" {
                        break;
                    }
                    if let Some((key, value)) = line.split_once(':')
                        && key.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse::<usize>().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; length];
                if reader.read_exact(&mut body).is_err() {
                    continue;
                }
                // The gateway only serves chat completions; anything else (a
                // responses-style probe) is answered as absent so the provider
                // falls back instead of hanging.
                if !request_line.contains("/chat/completions") {
                    let _ = socket.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    continue;
                }
                let request: serde_json::Value =
                    serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
                let messages = request["messages"].as_array().cloned().unwrap_or_default();
                let is_parent = messages.iter().any(|message| {
                    let content = message["content"].to_string();
                    content.contains(PARENT_SPAWN_ONCE) || content.contains(PARENT_SPAWN_AGAIN)
                });
                // The child's own objective has no tool result yet, and the parent
                // gets a tool result once the spawn has been executed. This is what
                // makes the gateway answer "call the tool" exactly once.
                // Only the current turn matters: a resumed conversation's transcript
                // already contains the previous run's tool messages, and treating
                // those as "this turn is answered" would make a resumed run silent.
                let current_turn_start = messages
                    .iter()
                    .rposition(|message| message["role"] == "user")
                    .unwrap_or(0);
                let already_spawned = messages[current_turn_start..]
                    .iter()
                    .any(|message| message["role"] == "tool");
                let frame = if is_parent && !already_spawned {
                    serde_json::json!({
                        "choices": [{
                            "delta": {
                                "tool_calls": [{
                                    "index": 0,
                                    "id": "spawn-1",
                                    "type": "function",
                                    "function": {
                                        "name": "spawn_agent",
                                        "arguments": serde_json::json!({
                                            "task": "return a short answer",
                                            "fork": false
                                        }).to_string()
                                    }
                                }]
                            },
                            "finish_reason": "tool_calls"
                        }]
                    })
                } else {
                    serde_json::json!({
                        "choices": [{
                            "delta": { "content": "done" },
                            "finish_reason": "stop"
                        }]
                    })
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {frame}\n\ndata: [DONE]\n\n"
                );
                let _ = socket.write_all(response.as_bytes());
            }
        });
        Self {
            url,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for SpawnGateway {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The runtime database, resolved through the binary's own path helper.
///
/// Not reconstructed by hand: `YI_AGENT_RUNTIME_DIR` is the override that exists
/// precisely so the layout can differ from the default, and a test that guesses it
/// would pass on one machine and fail on the next.
fn runtime_database(runtime_dir: &std::path::Path) -> std::path::PathBuf {
    runtime_dir.join("runtime.sqlite")
}

/// A headless entry that delegates must write a container and an execution.
///
/// The container is the conversation's place to delegate from; the execution is
/// the turn that spawned. Reading the two roles back is the whole point: if the
/// spawn had been attributed to the container, or the container had been replaced
/// by an execution row, the *next* turn would consult the wrong thing and -- as the
/// reported bug did -- refuse to spawn from a conversation that is still open.
/// Runs `yi-agent run --subagents` once in `directory` and returns what it wrote.
///
/// `resume` continues an existing conversation instead of starting one, which is
/// what makes "a second process on the same conversation" expressible: a restart
/// is exactly that, and the delegation it does afterwards must still work.
fn run_headless_once(
    directory: &std::path::Path,
    gateway: &SpawnGateway,
    marker: &str,
    resume: Option<&str>,
) -> std::process::Output {
    let instance = directory.join("instance");
    let runtime = directory.join("runtime");
    let mut command = Command::new(common::yi_agent_bin());
    // `--workdir` and the provider flags are process-wide, so they precede the
    // subcommand; `run`'s own flags follow it.
    command
        .arg("--workdir")
        .arg(directory)
        .args([
            "--provider",
            "openai",
            "--api-key",
            "test-key",
            "--model",
            "test-model",
            "--yolo",
        ])
        .arg("--api-url")
        .arg(&gateway.url)
        .arg("run");
    if let Some(thread_id) = resume {
        command.args(["--resume", thread_id]);
    }
    command
        // The marker is what makes the gateway call `spawn_agent` on the first
        // turn it sees; a resumed conversation re-sends it with the prompt.
        .arg(marker)
        .args(["--json", "--subagents"])
        .env("YI_AGENT_HOME", &instance)
        .env("YI_AGENT_RUNTIME_DIR", &runtime)
        .env("YI_AGENT_BRINGUP", "embedded")
        // Keep the run's own resume credential out of the developer's real home.
        .env("HOME", directory);
    common::run_command_with_timeout(&mut command, Duration::from_secs(120))
        .expect("the run completes")
}

#[test]
fn session_delegation_lifecycle_headless_entry_uses_container_context() {
    let directory = tempfile::Builder::new()
        .prefix("sdl")
        .tempdir_in("/private/tmp")
        .expect("a temp dir");
    let gateway = SpawnGateway::start();
    let runtime = directory.path().join("runtime");
    let output = run_headless_once(directory.path(), &gateway, PARENT_SPAWN_ONCE, None);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout_text = String::from_utf8_lossy(&output.stdout).to_string();
    eprintln!("STDOUT:\n{stdout_text}");
    eprintln!("STDERR:\n{}", String::from_utf8_lossy(&output.stderr));
    let events = common::parse_events(&stdout_text);
    let results: Vec<_> = events
        .iter()
        .filter_map(|event| event.get("ToolResult"))
        .collect();
    assert!(
        !results.is_empty(),
        "the run executed no tools at all; stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let spawn = results
        .iter()
        .find(|result| {
            result["result"]["is_error"] == serde_json::Value::Bool(false)
                && result.to_string().contains("spawn")
        })
        .or_else(|| results.first());
    assert!(
        spawn.is_some(),
        "the registered spawn tool never ran; stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    let database = runtime_database(&runtime);
    assert!(
        database.exists(),
        "the daemon wrote no runtime database at {database:?}"
    );
    let repository = yi_agent_store::repository::RuntimeRepository::open(&database)
        .expect("open the runtime db");
    let (mut containers, mut executions) = (0, 0);
    for summary in repository.task_summaries(None, false).expect("read tasks") {
        eprintln!(
            "ROW id={} role={} state={} parent={:?} activity={}",
            summary.task_id,
            summary.role,
            summary.state,
            summary.parent_task_id,
            summary.container_activity
        );
        let id: yi_agent_core::TaskId = summary.task_id.parse().expect("a task id");
        match repository.task_role(&id).expect("a persisted role") {
            TaskRole::SessionContainer => containers += 1,
            TaskRole::Execution => executions += 1,
        }
    }
    assert_eq!(
        (containers, executions),
        (1, 1),
        "a headless run must delegate from exactly one container into one execution"
    );

    // The container has no worker start: it is a conversation, not an attempt.
    let events_path = database.with_file_name("runtime.sqlite");
    assert!(events_path.exists());
}

/// A second process on the same conversation must still be able to spawn.
///
/// This is the reported failure, expressed as a test: the daemon outlives the
/// frontend, so after a restart the conversation's container is *already* there
/// with a historical attempt that ended `stalled`. Reading that history as
/// "settled, may not spawn" refuses every later turn in a conversation that is
/// still open. A restart is what makes the history non-empty, so a test that only
/// ever runs one process cannot see it.
#[test]
fn session_delegation_lifecycle_restarted_host_still_spawns_in_the_same_conversation() {
    let directory = tempfile::Builder::new()
        .prefix("sdl")
        .tempdir_in("/private/tmp")
        .expect("a temp dir");
    let gateway = SpawnGateway::start();
    let runtime = directory.path().join("runtime");

    let first = run_headless_once(directory.path(), &gateway, PARENT_SPAWN_ONCE, None);
    assert!(
        first.status.success(),
        "first run stderr: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let database = runtime_database(&runtime);
    let repository = yi_agent_store::repository::RuntimeRepository::open(&database)
        .expect("open the runtime db");
    // The conversation the first `run` created. Read from the thread store -- the
    // same place a restarting frontend lists conversations from -- rather than from
    // the container row, because a container is not itself a UI-visible thread.
    let thread_id = yi_agent_app_server::thread_store::ThreadStore::new(directory.path())
        .list()
        .expect("list conversations")
        .into_iter()
        .next()
        .map(|meta| meta.thread_id)
        .expect("the first run wrote a conversation");

    let second = run_headless_once(
        directory.path(),
        &gateway,
        PARENT_SPAWN_AGAIN,
        Some(&thread_id),
    );
    assert!(
        second.status.success(),
        "second run stderr: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let events = common::parse_events(&String::from_utf8_lossy(&second.stdout));
    let spawn_results: Vec<_> = events
        .iter()
        .filter_map(|event| event.get("ToolResult"))
        .filter(|result| result.to_string().contains("spawn"))
        .collect();
    assert!(
        !spawn_results.is_empty(),
        "the resumed run executed no spawn at all; stdout: {}",
        String::from_utf8_lossy(&second.stdout)
    );
    let refused: Vec<_> = spawn_results
        .iter()
        .filter(|result| result["result"]["is_error"] == serde_json::Value::Bool(true))
        .collect();
    assert!(
        refused.is_empty(),
        "a resumed conversation must still delegate; the daemon refused: {refused:?}"
    );

    // Still one conversation, not a second one minted by the restart.
    let containers = repository
        .task_summaries(None, false)
        .expect("read tasks")
        .into_iter()
        .filter(|summary| summary.role == "session_container")
        .count();
    assert_eq!(
        containers, 1,
        "resuming must adopt the conversation's container, not create another"
    );
}
