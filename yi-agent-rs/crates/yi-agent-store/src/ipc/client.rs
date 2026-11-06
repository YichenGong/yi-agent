use super::*;

pub(crate) const COMMAND_WRITE_DEADLINE: Duration = Duration::from_secs(30);

pub(crate) const CLIENT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Slack added on top of a request's own blocking window.
///
/// The budget must outlive the window *and* the round trip that follows it, or
/// the client gives up in the one moment the daemon is about to answer.
const REPLY_BUDGET_SLACK: Duration = Duration::from_secs(15);

/// How long a client should wait for this request's reply.
///
/// The default [`CLIENT_REQUEST_TIMEOUT`] is a *liveness* bound: it exists so a
/// wedged daemon cannot hang a caller forever. A request that legitimately asks
/// the daemon to block (`WaitAgent` carries its own `timeout_ms`) is slower than
/// that by design, so the generic bound would fire while the daemon is still
/// doing exactly what it was asked to do -- and the caller reads "still
/// working" as "the daemon is broken".
///
/// Deriving the budget from the request keeps the tight bound everywhere else:
/// only a request that declares a longer window gets a longer deadline. An
/// unbounded wait keeps the default, because no finite budget could cover it.
pub fn reply_budget(request: &IpcRequest) -> Duration {
    match request {
        IpcRequest::WaitAgent {
            timeout_ms: Some(window_ms),
            ..
        } => Duration::from_millis(*window_ms).saturating_add(REPLY_BUDGET_SLACK),
        _ => CLIENT_REQUEST_TIMEOUT,
    }
}

/// Maps a socket read deadline onto the timeout it actually is.
///
/// macOS reports a unix-socket read deadline as `EAGAIN`
/// ([`std::io::ErrorKind::WouldBlock`], `os error 35`) while Linux reports
/// `TimedOut`; both mean "no reply within the deadline". Passing `EAGAIN`
/// through unchanged made a plain deadline look like an opaque transport fault
/// -- and `IpcError::Io` is `#[error(transparent)]`, so the literal
/// "Resource temporarily unavailable (os error 35)" reached the model.
///
/// Only `WouldBlock` is rewritten: a genuine `TimedOut` is already correct, and
/// rewriting it would discard whatever detail the OS attached.
fn classify_reply_error(error: std::io::Error, deadline: Duration) -> IpcError {
    if error.kind() == std::io::ErrorKind::WouldBlock {
        return IpcError::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!(
                "the daemon did not answer within {}s (it may still be working)",
                deadline.as_secs_f32().round() as u64
            ),
        ));
    }
    IpcError::Io(error)
}

/// The longest socket path `sockaddr_un.sun_path` accepts, excluding the
/// terminating NUL. Linux allows 108; macOS and the BSDs allow 104. Taking the
/// smaller bound keeps one behaviour across platforms.
pub const MAX_SOCKET_PATH_BYTES: usize = 103;

/// A runtime directory normally owns its socket (`<runtime_dir>/runtime.sock`).
/// That layout cannot always work: `sun_path` is capped at
/// [`MAX_SOCKET_PATH_BYTES`], so a deep project path — the default runtime
/// directory is `<workdir>/.yi-agent/runtime` — overflows it and `bind` fails
/// with `AF_UNIX path too long`, silently disabling subagent delegation.
///
/// When the direct path does not fit, the socket moves to a short, stable name
/// under the platform temporary directory, derived from the runtime directory
/// so that the daemon and every client agree on one location. Distinct projects
/// keep distinct sockets; an explicitly shared runtime directory keeps sharing.
pub fn socket_path_for(runtime_dir: &Path) -> Result<PathBuf, IpcError> {
    let direct = runtime_dir.join("runtime.sock");
    if direct.as_os_str().len() <= MAX_SOCKET_PATH_BYTES {
        return Ok(direct);
    }
    let name = socket_file_name(runtime_dir);
    let fallback_dir = std::env::temp_dir();
    let fallback = fallback_dir.join(&name);
    if fallback.as_os_str().len() > MAX_SOCKET_PATH_BYTES {
        return Err(IpcError::SocketPathTooLong {
            path: fallback.display().to_string(),
            limit: MAX_SOCKET_PATH_BYTES,
        });
    }
    Ok(fallback)
}

/// The stable file name used when a socket cannot live beside its runtime
/// directory. Derived from the runtime directory, not the working directory, so
/// callers that share a runtime directory also share its socket.
pub(crate) fn socket_file_name(runtime_dir: &Path) -> String {
    let digest = Sha256::digest(runtime_dir.as_os_str().as_encoded_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("yi-agent-{}.sock", &hex[..16])
}

pub fn send_request(
    socket_path: impl AsRef<Path>,
    request: IpcRequest,
) -> Result<IpcResponse, IpcError> {
    send_request_with_version(socket_path, PROTOCOL_VERSION, request)
}

pub fn send_request_with_version(
    socket_path: impl AsRef<Path>,
    protocol_version: u32,
    request: IpcRequest,
) -> Result<IpcResponse, IpcError> {
    // The deadline follows the request: a plain query keeps the tight liveness
    // bound, while a request that asks the daemon to block gets a budget that
    // outlives its own window. Taking the default unconditionally here is what
    // turned every `wait_agent` longer than 30s into a transport failure while
    // the child was still running.
    let reply_timeout = reply_budget(&request);
    send_request_with_version_timeout(socket_path, protocol_version, request, reply_timeout)
}

/// `send_request_with_version` with an explicit reply deadline.
///
/// Factored out so the deadline is a *required argument* of the last hop rather
/// than an optional builder method a call site could forget, and so a test can
/// drive a near-instant deadline instead of waiting out the production one.
pub fn send_request_with_version_timeout(
    socket_path: impl AsRef<Path>,
    protocol_version: u32,
    request: IpcRequest,
    reply_timeout: Duration,
) -> Result<IpcResponse, IpcError> {
    let mut stream = UnixStream::connect(socket_path)?;
    // Bounded on the client too, not only on the server: a daemon that reads the
    // request and then never answers must not block the caller forever. A
    // timeout, like any other read error, surfaces through `read_limited_frame`
    // as a typed `IpcError`.
    stream.set_read_timeout(Some(reply_timeout))?;
    let request_id = next_request_id();
    write_request(&mut stream, protocol_version, request_id.clone(), request)?;
    let response = read_limited_frame(&mut BufReader::new(stream))
        .map_err(|error| match error {
            // A deadline that fired must read as a deadline, not as whatever
            // errno this platform happens to use for `EAGAIN`.
            IpcError::Io(io) => classify_reply_error(io, reply_timeout),
            other => other,
        })?
        .ok_or_else(|| {
            IpcError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "daemon closed request connection",
            ))
        })?;
    let envelope: ResponseEnvelope = serde_json::from_slice(&response)?;
    if envelope.protocol_version != PROTOCOL_VERSION || envelope.request_id != request_id {
        return Err(IpcError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "response envelope identity mismatch",
        )));
    }
    Ok(envelope.result)
}

/// Turns a daemon error frame into a real error.
///
/// `send_request` deliberately hands back whatever frame the daemon sent. The
/// trace helpers below cannot: their whole contract is "a snapshot or nothing",
/// so an error frame must not reach a caller that would then read an empty
/// trace as a healthy task.
pub(crate) fn into_result(response: IpcResponse) -> Result<IpcResponse, IpcError> {
    match response {
        IpcResponse::Error { code, message } => Err(IpcError::Remote {
            code,
            message: message.unwrap_or_else(|| code.to_string()),
        }),
        result => Ok(result),
    }
}

pub fn subscribe(
    socket_path: impl AsRef<Path>,
    after_event_id: i64,
) -> Result<Subscription, IpcError> {
    subscribe_with_filters(socket_path, after_event_id, SubscriptionFilters::default())
}

pub fn subscribe_with_filters(
    socket_path: impl AsRef<Path>,
    after_event_id: i64,
    filters: SubscriptionFilters,
) -> Result<Subscription, IpcError> {
    let mut stream = UnixStream::connect(socket_path)?;
    let request_id = next_request_id();
    write_request(
        &mut stream,
        PROTOCOL_VERSION,
        request_id.clone(),
        IpcRequest::SubscribeEvents {
            after_event_id,
            filters,
        },
    )?;
    Ok(Subscription {
        reader: BufReader::new(stream),
        request_id,
    })
}

pub(crate) fn write_request(
    stream: &mut UnixStream,
    protocol_version: u32,
    request_id: String,
    request: IpcRequest,
) -> Result<(), IpcError> {
    let frame = serde_json::to_vec(&RequestEnvelope {
        protocol_version,
        request_id,
        command: request,
    })?;
    if frame.len() > MAX_FRAME_BYTES {
        return Err(IpcError::FrameTooLarge);
    }
    stream.write_all(&frame)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

#[cfg(test)]
mod kanban_observation_frame_limit_tests {
    use super::*;

    /// 完整会话快照（`loaded_threads`）变大之后，请求体不能被 IPC **截断**。
    ///
    /// 截断比报错危险得多：服务端会解析出一个少了尾巴的 JSON，要么整条拒绝
    /// （看起来像插件不回答），要么在宽容解析下收下一个**假的**完整快照——
    /// 于是「热表里没有某个会话」变成一次凭空的结论。这里构造一个确定超过
    /// 1 MiB 的 `driver/next` 形状请求，断言客户端在写盘之前就报
    /// `FrameTooLarge`，而不是把半截帧发出去。
    #[test]
    fn kanban_observation_oversized_snapshot_is_refused_not_truncated() {
        // 每个条目约 40 字节，3 万个 ID 稳稳超过 1 MiB。
        let loaded: Vec<serde_json::Value> = (0..30_000)
            .map(|index| {
                serde_json::json!({
                    "thread_id": format!("thread-{index:012}"),
                    "idle": index % 2 == 0,
                })
            })
            .collect();
        let request = IpcRequest::PluginQuery {
            plugin: "superpowers-kanban".into(),
            method: "driver/next".into(),
            params: serde_json::json!({
                "threads": [],
                "loaded_threads": loaded,
                "results": [],
                "sessions": {"running": 0},
            }),
        };
        let (client, _server) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut client = client;
        let error = write_request(&mut client, PROTOCOL_VERSION, "req-1".into(), request)
            .expect_err("an oversized frame must be refused");
        assert!(
            matches!(error, IpcError::FrameTooLarge),
            "expected FrameTooLarge, got {error:?}"
        );
    }
}

#[cfg(test)]
mod late_reply_tests {
    use super::*;

    /// A reply that arrives after the deadline must be reported as a *timeout*,
    /// not as an opaque syscall error.
    ///
    /// macOS reports a unix-socket read deadline as `EAGAIN`
    /// (`ErrorKind::WouldBlock`, `os error 35`), and `IpcError::Io` is
    /// `#[error(transparent)]`, so the model used to see the literal
    /// "Resource temporarily unavailable (os error 35)". A caller holding a
    /// long-running request (a `wait_agent` that legitimately blocks for two
    /// minutes) read that as "the daemon is broken".
    #[test]
    fn a_reply_after_the_deadline_is_reported_as_a_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("runtime.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        // Accept, read the request, and stay busy: the daemon is *working*,
        // just not done within this client's budget.
        let server = std::thread::spawn(move || {
            let (mut stream, _addr) = listener.accept().unwrap();
            let mut discard = [0_u8; 128];
            let _ = std::io::Read::read(&mut stream, &mut discard);
            std::thread::sleep(Duration::from_secs(5));
        });

        let error = send_request_with_version_timeout(
            &socket,
            PROTOCOL_VERSION,
            IpcRequest::Status,
            Duration::from_millis(150),
        )
        .expect_err("a late reply must fail the client");

        match &error {
            IpcError::Io(io) => assert_eq!(
                io.kind(),
                std::io::ErrorKind::TimedOut,
                "a read deadline is a timeout, not a generic io failure: {error}"
            ),
            other => panic!("expected IpcError::Io, got {other:?}"),
        }
        let text = error.to_string();
        assert!(
            text.contains("did not answer within"),
            "the message must say the daemon did not answer in time: {text}"
        );
        assert!(
            !text.contains("os error"),
            "a raw errno is an implementation detail, not a model-visible fact: {text}"
        );
        drop(server);
    }
}

#[cfg(test)]
mod reply_budget_tests {
    use super::*;

    fn wait(timeout_ms: Option<u64>) -> IpcRequest {
        IpcRequest::WaitAgent {
            session_id: "s".into(),
            caller_task_id: "t".into(),
            capability: "c".into(),
            mode: "any".into(),
            timeout_ms,
        }
    }

    /// A client deadline must never fire before the request's own budget does.
    ///
    /// `wait_agent` asks the daemon to block for up to two minutes, but the
    /// shared client deadline is 30 seconds. Every wait longer than that came
    /// back as "Resource temporarily unavailable (os error 35)" while the child
    /// was still working: "still running" was being reported as "the daemon is
    /// broken".
    #[test]
    fn a_wait_budget_outlives_the_window_the_daemon_blocks_for() {
        let window = Duration::from_millis(120_000);
        let budget = reply_budget(&wait(Some(120_000)));
        assert!(
            budget > window,
            "the client must still be waiting when the daemon's window ends: {budget:?}"
        );
    }

    /// An unbounded wait cannot be covered by any deadline, so it keeps the
    /// default: this method must not silently unbound the client.
    #[test]
    fn an_unbounded_wait_keeps_the_default_budget() {
        assert_eq!(reply_budget(&wait(None)), CLIENT_REQUEST_TIMEOUT);
    }

    /// The budget is derived from the request, not a blanket raise: a plain
    /// query keeps the shared default so a wedged daemon still fails fast.
    #[test]
    fn a_plain_request_keeps_the_default_budget() {
        assert_eq!(reply_budget(&IpcRequest::Status), CLIENT_REQUEST_TIMEOUT);
    }
}

#[cfg(test)]
mod socket_path_tests {
    use super::*;

    /// The real project path that produced `AF_UNIX path too long` in practice:
    /// `<repo>/.yi-agent/runtime/runtime.sock` measured 108 bytes.
    const LONG_RUNTIME_DIR: &str = "/Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.yi-agent/runtime";

    #[test]
    fn a_short_runtime_directory_keeps_the_socket_inside_it() {
        let runtime_dir = Path::new("/tmp/project/.yi-agent/runtime");

        let socket = socket_path_for(runtime_dir).expect("short paths resolve");

        assert_eq!(
            socket,
            runtime_dir.join("runtime.sock"),
            "a runtime directory that fits must keep its existing layout"
        );
    }

    #[test]
    fn a_long_runtime_directory_yields_a_socket_that_fits_the_platform_limit() {
        let runtime_dir = Path::new(LONG_RUNTIME_DIR);
        assert!(
            runtime_dir.join("runtime.sock").as_os_str().len() > MAX_SOCKET_PATH_BYTES,
            "precondition: the direct socket path must overflow the limit"
        );

        let socket = socket_path_for(runtime_dir).expect("long paths fall back, not fail");

        assert!(
            socket.as_os_str().len() <= MAX_SOCKET_PATH_BYTES,
            "socket must fit sockaddr_un.sun_path, got {} bytes: {}",
            socket.as_os_str().len(),
            socket.display()
        );
    }

    #[test]
    fn the_fallback_is_deterministic_for_one_runtime_directory() {
        let runtime_dir = Path::new(LONG_RUNTIME_DIR);

        let first = socket_path_for(runtime_dir).expect("resolves");
        let second = socket_path_for(runtime_dir).expect("resolves");

        assert_eq!(
            first, second,
            "the daemon and every client must resolve the identical socket"
        );
    }

    #[test]
    fn distinct_runtime_directories_get_distinct_sockets() {
        let one = socket_path_for(Path::new(LONG_RUNTIME_DIR)).expect("resolves");
        let other =
            socket_path_for(Path::new(&format!("{LONG_RUNTIME_DIR}-other"))).expect("resolves");

        assert_ne!(
            one, other,
            "project isolation must survive the fallback: different projects need different sockets"
        );
    }

    #[test]
    fn a_shared_runtime_directory_resolves_to_one_socket() {
        // An explicitly shared runtime directory must still yield a single socket,
        // so two projects pointed at it attach to the same daemon.
        let shared = Path::new(
            "/Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/shared-runtime-dir/runtime",
        );

        let first = socket_path_for(shared).expect("resolves");
        let second = socket_path_for(shared).expect("resolves");

        assert_eq!(first, second);
    }
}

#[cfg(test)]
mod socket_identity_tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::net::UnixListener;

    fn identity(path: &Path) -> (u64, u64) {
        let metadata = fs::symlink_metadata(path).expect("node must exist");
        (metadata.dev(), metadata.ino())
    }

    #[test]
    fn a_socket_node_we_still_own_is_removed() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("own.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let recorded = identity(&path);

        remove_socket_if_same_inode(&path, Some(recorded));

        assert!(!path.exists(), "our own socket node must be cleaned up");
        drop(listener);
    }

    #[test]
    fn a_replacement_socket_at_the_same_path_is_left_alone() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("reused.sock");
        let first = UnixListener::bind(&path).unwrap();
        let stale_identity = identity(&path);
        // 换掉节点：unlink 后重新 bind，拿到一个 (dev, ino) 不同的新节点。
        // 注意 drop(listener) 本身**不会** unlink 文件——必须先显式删。
        drop(first);
        std::fs::remove_file(&path).unwrap();
        let _replacement = UnixListener::bind(&path).unwrap();
        assert_ne!(
            stale_identity,
            identity(&path),
            "precondition: rebinding the same path must yield a new inode"
        );

        remove_socket_if_same_inode(&path, Some(stale_identity));

        assert!(
            path.exists(),
            "a socket node bound by a replacement daemon must survive the outgoing daemon's cleanup"
        );
    }

    #[test]
    fn an_unknown_identity_removes_nothing() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("unknown.sock");
        let _listener = UnixListener::bind(&path).unwrap();

        remove_socket_if_same_inode(&path, None);

        assert!(
            path.exists(),
            "without a recorded identity the cleanup must not guess"
        );
    }

    #[test]
    fn a_vanished_node_is_not_an_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("absent.sock");

        remove_socket_if_same_inode(&path, Some((0, 0)));

        assert!(!path.exists());
    }
}

#[cfg(test)]
mod client_request_timeout_tests {
    use super::*;

    /// A client must never block forever on a daemon that accepts the
    /// connection but never answers. The server side already bounds its reads
    /// (1s) and writes (30s), and the plugin hop bounds both; the client->daemon
    /// hop was the only one left unbounded, so a wedged daemon turned into an
    /// infinite hang for every caller (and for any tokio thread that called it).
    #[test]
    fn a_daemon_that_never_answers_fails_the_client_fast() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("runtime.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        // Accept in the background and then deliberately say nothing: this is a
        // daemon that is up (connect succeeds) but wedged (no reply).
        let server = std::thread::spawn(move || {
            let (_stream, _addr) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_secs(30));
        });

        let started = Instant::now();
        let result = send_request_with_version_timeout(
            &socket,
            PROTOCOL_VERSION,
            IpcRequest::Status,
            Duration::from_millis(150),
        );
        let elapsed = started.elapsed();

        assert!(
            result.is_err(),
            "a silent daemon must surface an error, not block: {result:?}"
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "the client must give up promptly, took {elapsed:?}"
        );
        drop(server);
    }
}
