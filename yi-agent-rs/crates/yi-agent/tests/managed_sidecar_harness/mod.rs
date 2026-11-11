//! Bounded pipes for a real sidecar child, and a scripted local model gateway.
//!
//! Two problems this module exists for, both observed as *hangs* rather than as
//! failures before it existed:
//!
//! * **A silent child must not hang the test.** `read_line` on a pipe blocks until
//!   a newline arrives, and a deadline checked only *after* a successful read can
//!   never fire while the child says nothing. [`BoundedPipes::next_frame`] therefore
//!   waits on a bounded channel with a real timeout, so a silent sidecar produces
//!   an error naming the deadline instead of an unbounded wait.
//! * **A chatty child must not block on a full pipe.** The sidecar writes both its
//!   protocol frames and its tracing output to descriptors this test owns. Nobody
//!   reading stderr means a full stderr pipe blocks the sidecar mid-turn, and a
//!   frame longer than the reader's buffer grows memory without bound. So stderr is
//!   drained on its own thread, and one frame is capped at
//!   [`MAX_FRAME_BYTES`].

#![allow(dead_code)]

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Largest single protocol frame this harness will accept.
///
/// A frame is one JSON line. 4 MiB is far above any real frame even with a full
/// transcript attached, and far below the memory a runaway writer could consume.
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// How many parsed frames may sit between the reader thread and the test.
///
/// The channel is bounded on purpose: a test that stops reading must slow the
/// child down (pipe backpressure) rather than let the harness buffer megabytes it
/// will never look at.
const FRAME_QUEUE: usize = 512;

/// How much stderr is kept for diagnostics. Older bytes are dropped; this is a
/// tail for failure messages, not a transcript.
const STDERR_TAIL_BYTES: usize = 128 * 1024;

/// The two descriptors of a live child, read and written under bounded rules.
pub struct BoundedPipes {
    stdin: ChildStdin,
    frames: Receiver<std::io::Result<String>>,
    stderr_tail: Arc<Mutex<VecDeque<u8>>>,
    closed: Arc<AtomicBool>,
}

impl BoundedPipes {
    /// Takes the child's stdin/stdout/stderr and starts the two reader threads.
    ///
    /// Must be called exactly once per child: the threads own the descriptors.
    pub fn attach(child: &mut Child) -> std::io::Result<Self> {
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("the child has no stdin pipe"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("the child has no stdout pipe"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| std::io::Error::other("the child has no stderr pipe"))?;
        let closed = Arc::new(AtomicBool::new(false));
        let (tx, frames) = mpsc::sync_channel::<std::io::Result<String>>(FRAME_QUEUE);
        spawn_frame_reader(stdout, tx, Arc::clone(&closed));
        let stderr_tail = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL_BYTES)));
        spawn_stderr_drain(stderr, Arc::clone(&stderr_tail));
        set_nonblocking(&stdin)?;
        Ok(Self {
            stdin,
            frames,
            stderr_tail,
            closed,
        })
    }

    /// Writes one line, bounded by `budget`.
    ///
    /// A blocking `write` on a pipe whose reader stopped returns only when the
    /// reader drains or the descriptor breaks. The deadline is what turns that into
    /// a reportable failure.
    pub fn write_line(&mut self, line: &str, budget: Duration) -> std::io::Result<()> {
        let mut bytes = Vec::with_capacity(line.len() + 1);
        bytes.extend_from_slice(line.as_bytes());
        bytes.push(b'\n');
        write_all_with_deadline(&mut self.stdin, &bytes, budget)
    }

    /// Returns the next non-empty line, or an error naming `budget` on timeout.
    pub fn next_frame(&mut self, budget: Duration) -> std::io::Result<String> {
        match self.frames.recv_timeout(budget) {
            Ok(Ok(line)) => Ok(line),
            Ok(Err(error)) => Err(error),
            Err(RecvTimeoutError::Timeout) => Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("the child produced no frame within {budget:?}"),
            )),
            Err(RecvTimeoutError::Disconnected) => Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the child closed its stdout before answering",
            )),
        }
    }

    /// Whether the reader side has ended (EOF or an oversized line).
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// The trailing stderr bytes, for failure messages.
    pub fn stderr_tail(&self) -> String {
        let tail = self
            .stderr_tail
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let bytes: Vec<u8> = tail.iter().copied().collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

/// Reads newline-delimited frames, bounded in both length and depth.
fn spawn_frame_reader(
    stdout: ChildStdout,
    tx: SyncSender<std::io::Result<String>>,
    closed: Arc<AtomicBool>,
) {
    std::thread::Builder::new()
        .name("sidecar-frame-reader".into())
        .spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut buffer: Vec<u8> = Vec::new();
            loop {
                buffer.clear();
                // `take` caps one read at MAX+1 bytes, so a writer that never emits
                // a newline cannot grow this buffer past the cap.
                let read = {
                    let mut limited = (&mut reader).take(MAX_FRAME_BYTES as u64 + 1);
                    limited.read_until(b'\n', &mut buffer)
                };
                match read {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(error) => {
                        let _ = tx.send(Err(error));
                        break;
                    }
                }
                if buffer.len() > MAX_FRAME_BYTES {
                    let _ = tx.send(Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "a frame exceeded the {MAX_FRAME_BYTES}-byte cap; the reader \
                             refuses to grow without bound"
                        ),
                    )));
                    break;
                }
                // A trailing fragment with no newline is a truncated frame, not a
                // frame. Delivering it would hand the test half a JSON document that
                // then fails to parse for the wrong reason (or, worse, parses into a
                // partial value if the writer happened to stop at a `}`).
                if buffer.last() != Some(&b'\n') {
                    let _ = tx.send(Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        format!(
                            "the child's stdout ended mid-frame after {} bytes",
                            buffer.len()
                        ),
                    )));
                    break;
                }
                let text = String::from_utf8_lossy(&buffer);
                let text = text.trim();
                if text.is_empty() {
                    continue;
                }
                if tx.send(Ok(text.to_string())).is_err() {
                    // The test dropped its receiver; nobody is listening any more.
                    break;
                }
            }
            closed.store(true, Ordering::SeqCst);
            // Dropping `tx` here is what turns "child closed stdout" into a
            // `Disconnected` for whoever is waiting.
        })
        .expect("spawn the frame reader thread");
}

/// Drains stderr forever, keeping only the tail.
///
/// The draining is the point: an unread pipe fills and then blocks the child.
fn spawn_stderr_drain(stderr: ChildStderr, tail: Arc<Mutex<VecDeque<u8>>>) {
    std::thread::Builder::new()
        .name("sidecar-stderr-drain".into())
        .spawn(move || {
            let mut stderr = stderr;
            let mut chunk = [0u8; 8192];
            loop {
                match stderr.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => {
                        let mut tail = tail.lock().unwrap_or_else(|p| p.into_inner());
                        for byte in &chunk[..read] {
                            if tail.len() == STDERR_TAIL_BYTES {
                                tail.pop_front();
                            }
                            tail.push_back(*byte);
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        })
        .expect("spawn the stderr drain thread");
}

fn set_nonblocking(stdin: &ChildStdin) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let fd = stdin.as_raw_fd();
    // SAFETY: `fd` is an open descriptor owned by `stdin`; `F_GETFL`/`F_SETFL` only
    // read and set its status flags.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: same descriptor, same reason.
    let set = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if set < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Writes every byte, waiting for writability with `poll` until `budget` expires.
fn write_all_with_deadline(
    stdin: &mut ChildStdin,
    mut bytes: &[u8],
    budget: Duration,
) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let fd = stdin.as_raw_fd();
    let deadline = Instant::now() + budget;
    while !bytes.is_empty() {
        // SAFETY: `fd` is open and owned by `stdin`; `bytes` is valid for its length.
        let written =
            unsafe { libc::write(fd, bytes.as_ptr().cast::<libc::c_void>(), bytes.len()) };
        if written > 0 {
            bytes = &bytes[written as usize..];
            continue;
        }
        if written == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "the child's stdin accepted no bytes",
            ));
        }
        let error = std::io::Error::last_os_error();
        match error.kind() {
            std::io::ErrorKind::Interrupted => continue,
            // macOS reports a full non-blocking pipe as `WouldBlock`; some paths
            // surface `TimedOut`. Both mean "not writable now".
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(write_deadline_error(budget));
                }
                let mut pollfd = libc::pollfd {
                    fd,
                    events: libc::POLLOUT,
                    revents: 0,
                };
                let millis = remaining.as_millis().min(i32::MAX as u128) as i32;
                // SAFETY: one valid `pollfd`, and the timeout is a plain integer.
                let ready = unsafe { libc::poll(&mut pollfd, 1, millis) };
                if ready < 0 {
                    let poll_error = std::io::Error::last_os_error();
                    if poll_error.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(poll_error);
                }
                if ready == 0 {
                    return Err(write_deadline_error(budget));
                }
                // Readiness (including POLLHUP) means the next write reports the
                // real outcome; loop so it does.
            }
            _ => return Err(error),
        }
    }
    Ok(())
}

fn write_deadline_error(budget: Duration) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!("the child did not accept a frame within {budget:?}"),
    )
}

// ---------------------------------------------------------------------------
// A scripted OpenAI-compatible gateway.
// ---------------------------------------------------------------------------

/// One provider request the gateway saw, recorded for assertions.
#[derive(Clone, Debug, Default)]
pub struct ModelTurn {
    /// The `model` field, which is what separates the parent from its children:
    /// both sides are offered `spawn_agent`, so tool presence proves nothing.
    pub model: String,
    /// Every `role:"tool"` body in the request, in order.
    pub tool_results: Vec<String>,
    /// Names of the tools the request offered.
    pub tool_names: Vec<String>,
    /// The role of the last message: this is what step of the script we are on.
    pub last_role: String,
}

/// Two catalog entries and one script, shared with the assertion side.
#[derive(Clone)]
pub struct GatewayScript {
    pub parent_model: String,
    pub child_model: String,
    pub turns: Arc<Mutex<Vec<ModelTurn>>>,
    /// Requests that did not arrive on `/chat/completions`, in order.
    ///
    /// The catalog entries pin `api_style: "chat"`; anything else here means the
    /// scripted provider binding is wrong, and answering it anyway would silently
    /// add turns the assertions never expected.
    pub unexpected_paths: Arc<Mutex<Vec<String>>>,
}

impl GatewayScript {
    pub fn new(parent_model: &str, child_model: &str) -> Self {
        Self {
            parent_model: parent_model.to_string(),
            child_model: child_model.to_string(),
            turns: Arc::new(Mutex::new(Vec::new())),
            unexpected_paths: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn turns(&self) -> Vec<ModelTurn> {
        self.turns
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Requests attributed to the parent model.
    pub fn parent_turns(&self) -> Vec<ModelTurn> {
        self.turns()
            .into_iter()
            .filter(|turn| turn.model == self.parent_model)
            .collect()
    }

    /// Every request path that was not `/chat/completions`.
    pub fn unexpected_paths(&self) -> Vec<String> {
        self.unexpected_paths
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Requests attributed to the child entry (`subagent_model`).
    pub fn child_turns(&self) -> Vec<ModelTurn> {
        self.turns()
            .into_iter()
            .filter(|turn| turn.model == self.child_model)
            .collect()
    }
}

/// A running scripted gateway, owned by the test that started it.
///
/// The thread exits on its own at drop: the stop flag is held *here*, the accept
/// loop polls it with a bounded sleep, and `stop` joins with a deadline. A thread
/// nobody can stop would outlive the test that started it and keep the listening
/// socket alive, so a later connect would succeed against a graveyard.
pub struct GatewayHandle {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl GatewayHandle {
    /// Stops accepting, waits for the thread, and gives up after `budget`.
    ///
    /// Called explicitly by the test and again from `Drop`, so a panic in the test
    /// body still ends the thread.
    pub fn stop(&mut self, budget: Duration) {
        self.stop_flag();
        let Some(thread) = self.thread.take() else {
            return;
        };
        let deadline = Instant::now() + budget;
        while !thread.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        if thread.is_finished() {
            let _ = thread.join();
        }
        // A thread still inside `serve_one` is bounded by its own read/write
        // timeouts, so detaching it is better than blocking a failing test for
        // another 30 s.
    }

    fn stop_flag(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl Drop for GatewayHandle {
    fn drop(&mut self) {
        self.stop(Duration::from_secs(5));
    }
}

/// Serves the two-entry script until the returned handle is stopped.
///
/// Each request is answered with a full HTTP response: headers, an exact
/// `Content-Length`, then an SSE body. `Connection: close` keeps every request on
/// its own connection, so the script never has to parse a pipelined stream.
pub fn spawn_scripted_gateway(
    listener: std::net::TcpListener,
    script: GatewayScript,
) -> GatewayHandle {
    listener
        .set_nonblocking(true)
        .expect("arm the gateway listener");
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let thread = std::thread::Builder::new()
        .name("scripted-model-gateway".into())
        .spawn(move || {
            while !thread_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        // One connection at a time is enough: the provider issues one
                        // request at a time per worker, and serialising keeps the
                        // turn order in `script.turns` meaningful.
                        let _ = stream.set_nonblocking(false);
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                        let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                        if let Err(error) = serve_one(&mut stream, &script) {
                            eprintln!("[gateway] request failed: {error}");
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        })
        .expect("spawn the model gateway thread");
    GatewayHandle {
        stop,
        thread: Some(thread),
    }
}

/// Reads one bounded HTTP request and answers it from the script.
fn serve_one(stream: &mut std::net::TcpStream, script: &GatewayScript) -> std::io::Result<()> {
    let request = read_http_request(stream)?;
    // The catalog pins `api_style: "chat"`, so `/chat/completions` is the only
    // path a correct configuration can produce. Answering anything else would
    // silently add script turns the assertions never accounted for; recording the
    // path and refusing makes a mis-pinned `api_style` a visible failure instead.
    if request.path != "/chat/completions" {
        script
            .unexpected_paths
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(request.path.clone());
        return write_http(stream, 404, "text/plain", b"no such route");
    }
    let Some(body) = request.body else {
        return write_http(stream, 400, "text/plain", b"missing body");
    };
    let value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => {
            let message = format!("the gateway could not parse the request body: {error}");
            return write_http(stream, 400, "text/plain", message.as_bytes());
        }
    };
    let model = value
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let tool_names: Vec<String> = value
        .get("tools")
        .and_then(|v| v.as_array())
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| {
                    tool.get("function")
                        .and_then(|f| f.get("name"))
                        .or_else(|| tool.get("name"))
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default();
    let messages = value
        .get("messages")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let tool_results: Vec<String> = messages
        .iter()
        .filter(|message| message.get("role").and_then(|r| r.as_str()) == Some("tool"))
        .filter_map(|message| message.get("content").and_then(|c| c.as_str()))
        .map(str::to_string)
        .collect();
    let last_role = messages
        .last()
        .and_then(|message| message.get("role"))
        .and_then(|role| role.as_str())
        .unwrap_or_default()
        .to_string();

    script
        .turns
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(ModelTurn {
            model: model.clone(),
            tool_results: tool_results.clone(),
            tool_names: tool_names.clone(),
            last_role: last_role.clone(),
        });

    let frames = script_frames(script, &model, &tool_names, &tool_results, &last_role);
    let mut out = String::from(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/event-stream\r\n\
         Cache-Control: no-cache\r\n\
         Connection: close\r\n",
    );
    let body = frames
        .iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect::<String>()
        + "data: [DONE]\n\n";
    out.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    out.push_str(&body);
    stream.write_all(out.as_bytes())?;
    stream.flush()
}

/// The script: what each side says at each step.
///
/// Deliberately keyed on `model` and on the last message role, not on which tools
/// exist. A read-only child is offered the same delegation tools its parent has
/// (`DaemonAgentWorkerFactory` registers them for every worker), so keying on
/// `spawn_agent` would make the child take the parent's branch and the test would
/// pass while proving nothing.
fn script_frames(
    script: &GatewayScript,
    model: &str,
    tool_names: &[String],
    tool_results: &[String],
    last_role: &str,
) -> Vec<String> {
    // A title request carries no tools. Answering it with a tool call would make
    // the title model look like a chat participant.
    if tool_names.is_empty() {
        return text_frames("Fixture title");
    }
    let is_child = model == script.child_model;
    let is_parent = model == script.parent_model;
    if is_child {
        return match last_role {
            "user" => bash_frames("echo hello", "child-bash"),
            _ => text_frames("child finished"),
        };
    }
    if is_parent {
        return match last_role {
            "user" => spawn_frames("Delegate a greeting to one read-only child."),
            "tool" => {
                let last = tool_results.last().map(String::as_str).unwrap_or_default();
                if last.contains("\"status\":\"queued\"") {
                    wait_frames()
                } else {
                    text_frames("parent done")
                }
            }
            _ => text_frames("parent done"),
        };
    }
    // An unknown model is a script bug, and a tool call would hide it as a hanging
    // turn. Say so in the text so the assertion points at the catalog.
    text_frames(&format!("unexpected model {model}"))
}

fn text_frames(text: &str) -> Vec<String> {
    let delta = serde_json::json!({
        "choices": [{ "delta": { "content": text }, "index": 0 }],
    });
    let stop = serde_json::json!({
        "choices": [{ "delta": {}, "finish_reason": "stop", "index": 0 }],
    });
    vec![delta.to_string(), stop.to_string()]
}

/// An assistant turn that calls one tool, streamed the way a real provider does.
fn tool_call_frames(name: &str, id: &str, arguments: serde_json::Value) -> Vec<String> {
    let head = serde_json::json!({
        "choices": [{
            "delta": { "tool_calls": [{
                "index": 0,
                "id": id,
                "type": "function",
                "function": { "name": name, "arguments": "" },
            }] },
            "index": 0,
        }],
    });
    let body = serde_json::json!({
        "choices": [{
            "delta": { "tool_calls": [{
                "index": 0,
                "function": { "arguments": arguments.to_string() },
            }] },
            "index": 0,
        }],
    });
    let stop = serde_json::json!({
        "choices": [{ "delta": {}, "finish_reason": "tool_calls", "index": 0 }],
    });
    vec![head.to_string(), body.to_string(), stop.to_string()]
}

fn spawn_frames(task: &str) -> Vec<String> {
    tool_call_frames(
        "spawn_agent",
        "call-spawn",
        serde_json::json!({
            "task": task,
            "mode": "read_only",
            "fork": false,
        }),
    )
}

fn wait_frames() -> Vec<String> {
    tool_call_frames(
        "wait_agent",
        "call-wait",
        serde_json::json!({ "mode": "all" }),
    )
}

fn bash_frames(command: &str, id: &str) -> Vec<String> {
    tool_call_frames("bash", id, serde_json::json!({ "command": command }))
}

struct HttpRequest {
    /// The origin-form target of the request line, e.g. `/chat/completions`.
    path: String,
    body: Option<Vec<u8>>,
}

/// Reads one request with a bounded header block and an exact-length body.
///
/// The length comes from the `Content-Length` header and is capped: a body is not
/// read until EOF, because a client that keeps the connection open would hang the
/// gateway.
fn read_http_request(stream: &mut std::net::TcpStream) -> std::io::Result<HttpRequest> {
    const MAX_HEADER_BYTES: usize = 64 * 1024;
    const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(position) = find_subsequence(&buffer, b"\r\n\r\n") {
            break position + 4;
        }
        if buffer.len() > MAX_HEADER_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "the request header block exceeded its cap",
            ));
        }
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            if buffer.is_empty() {
                return Ok(HttpRequest {
                    path: String::new(),
                    body: None,
                });
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the connection closed inside the request header",
            ));
        }
        buffer.extend_from_slice(&chunk[..read]);
    };

    let header = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
    // `METHOD /path HTTP/1.1` — only the target is needed, and only to tell a
    // correct `/chat/completions` binding from a mis-pinned `api_style`.
    let path = header
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or_default()
        .to_string();
    let mut content_length = 0usize;
    let mut chunked = false;
    for line in header.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.trim().parse::<usize>().unwrap_or(0);
        } else if name.eq_ignore_ascii_case("transfer-encoding")
            && value.to_ascii_lowercase().contains("chunked")
        {
            chunked = true;
        }
    }
    if chunked {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "a chunked request body is not supported by the fixture gateway",
        ));
    }
    if content_length > MAX_BODY_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("the request body declared {content_length} bytes, above the cap"),
        ));
    }

    let mut body = buffer[header_end..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the connection closed before the declared body length arrived",
            ));
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);
    Ok(HttpRequest {
        path,
        body: Some(body),
    })
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn write_http(
    stream: &mut std::net::TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: {content_type}\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

// ---------------------------------------------------------------------------
// The bounded IO contract, proved against real child processes.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    /// A child the harness fully controls: `sh -c <script>` with no inherited
    /// descriptors beyond the three pipes.
    fn spawn_script(script: &str) -> (Child, BoundedPipes) {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn the scripted child");
        let pipes = BoundedPipes::attach(&mut child).expect("attach bounded pipes");
        (child, pipes)
    }

    #[test]
    fn a_silent_child_fails_its_deadline_instead_of_blocking_forever() {
        let (mut child, mut pipes) = spawn_script("sleep 30");
        let started = Instant::now();
        let error = pipes
            .next_frame(Duration::from_millis(400))
            .expect_err("a silent child must not answer");
        let elapsed = started.elapsed();
        assert!(
            error.to_string().contains("within"),
            "the error must name the deadline: {error}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "the deadline must fire in real time, took {elapsed:?}"
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn a_partial_frame_without_a_newline_is_still_bounded_by_the_deadline() {
        // Half a JSON document and then silence, with the pipe still open. A reader
        // that treats "some bytes arrived" as "a frame arrived" would hand the test a
        // truncated document; one that waits for a newline must still honour the
        // deadline.
        let (mut child, mut pipes) = spawn_script("printf '{\"jsonrpc\":\"2.0\"'; sleep 30");
        let started = Instant::now();
        let error = pipes
            .next_frame(Duration::from_millis(400))
            .expect_err("a partial frame must not answer");
        assert!(
            error.to_string().contains("within"),
            "the error must name the deadline: {error}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the deadline must fire in real time"
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn a_frame_truncated_by_stdout_eof_is_an_error_not_a_frame() {
        // The writer stopped mid-frame and closed. The fragment must never be
        // delivered as if it were complete.
        let (mut child, mut pipes) = spawn_script("printf '{\"jsonrpc\":\"2.0\"}'");
        let error = pipes
            .next_frame(Duration::from_secs(5))
            .expect_err("a truncated frame must not be delivered");
        assert!(
            error.to_string().contains("mid-frame"),
            "the error must say the frame was truncated: {error}"
        );
        let _ = child.wait();
    }

    #[test]
    fn an_oversized_frame_is_refused_instead_of_growing_without_bound() {
        // One line far past the cap, then a valid one. The reader must report the
        // cap rather than buffer the whole line.
        let (mut child, mut pipes) = spawn_script(&format!(
            "head -c {} /dev/zero | tr '\\0' 'a'; printf '\\n'; printf '{{\"ok\":true}}\\n'; sleep 5",
            MAX_FRAME_BYTES + 1024
        ));
        let error = pipes
            .next_frame(Duration::from_secs(20))
            .expect_err("an oversized frame must not be buffered");
        assert!(
            error.to_string().contains("cap"),
            "the error must name the cap: {error}"
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn a_child_that_stops_reading_trips_the_write_deadline() {
        // The child never reads stdin, so the pipe fills and stays full.
        let (mut child, mut pipes) = spawn_script("sleep 30");
        let oversized = "x".repeat(4 * 1024 * 1024);
        let error = pipes
            .write_line(&oversized, Duration::from_millis(500))
            .expect_err("a full pipe must not accept a frame forever");
        assert!(
            error.to_string().contains("within"),
            "the error must name the deadline: {error}"
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn stderr_is_drained_so_a_chatty_child_still_answers() {
        // 4 MiB of stderr is many pipe-fulls: without a drain the child would block
        // writing it and never reach the frame that follows.
        let (mut child, mut pipes) = spawn_script(
            "head -c 4194304 /dev/zero | tr '\\0' 'e' >&2; printf '{\"ok\":true}\\n'; sleep 5",
        );
        let frame = pipes
            .next_frame(Duration::from_secs(20))
            .expect("a child whose stderr is drained must still answer");
        assert_eq!(frame, "{\"ok\":true}");
        assert_eq!(pipes.stderr_tail().len(), STDERR_TAIL_BYTES);
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn a_closed_stdout_is_reported_as_eof_not_as_a_timeout() {
        let (mut child, mut pipes) = spawn_script("exit 0");
        let error = pipes
            .next_frame(Duration::from_secs(5))
            .expect_err("a finished child has nothing to say");
        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof, "{error}");
        assert!(pipes.is_closed(), "EOF must be recorded as closed");
        let _ = child.wait();
    }

    #[test]
    fn the_gateway_reads_an_exact_length_body_and_answers_with_sse() {
        use std::io::Read as _;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        let script = GatewayScript::new("parent-model", "child-model");
        let _gateway = spawn_scripted_gateway(listener, script.clone());

        let body = serde_json::json!({
            "model": "parent-model",
            "messages": [{ "role": "user", "content": "hi" }],
            "tools": [{ "type": "function", "function": { "name": "spawn_agent" } }],
        })
        .to_string();
        let mut stream = std::net::TcpStream::connect(address).expect("connect");
        let request = format!(
            "POST /chat/completions HTTP/1.1\r\nHost: 127.0.0.1\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        stream
            .write_all(request.as_bytes())
            .expect("write the request");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read response");
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("text/event-stream"), "{response}");
        assert!(
            response.contains("spawn_agent"),
            "the script must answer with a spawn: {response}"
        );

        let turns = script.turns();
        assert_eq!(turns.len(), 1, "{turns:?}");
        assert_eq!(turns[0].model, "parent-model");
        assert_eq!(turns[0].last_role, "user");
        assert!(turns[0].tool_names.contains(&"spawn_agent".to_string()));
    }
}
