use super::*;

pub(crate) const COMMAND_WRITE_DEADLINE: Duration = Duration::from_secs(30);

pub(crate) const CLIENT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

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
    send_request_with_version_timeout(
        socket_path,
        protocol_version,
        request,
        CLIENT_REQUEST_TIMEOUT,
    )
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
    let response = read_limited_frame(&mut BufReader::new(stream))?.ok_or_else(|| {
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
