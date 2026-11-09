//! Status 探活预算的进程级测试：真实 Unix socket、真实客户端入口。
//!
//! 这里只用公开 API（[`send_status_with_budget`]）；需要绕过公开入口的小请求
//! （部分写、SIGPIPE）留在 `src/status_io.rs` 的单元测试里。

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use superpowers_kanban_ipc::client::{ClientError, ProbeStage, send_status_with_budget};
use superpowers_kanban_ipc::wire::{PROTOCOL_VERSION, Reply, RequestEnvelope, ResponseEnvelope};

fn short_root(prefix: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in("/tmp")
        .expect("short temp root")
}

/// 读一行请求，用它的 request_id 回一帧合法应答。
fn reply_status(stream: &mut UnixStream, result: Reply) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).expect("read one request line");
    let request: RequestEnvelope = serde_json::from_str(line.trim()).expect("request is JSON");
    let envelope = ResponseEnvelope {
        protocol_version: PROTOCOL_VERSION,
        request_id: request.request_id,
        event_id: None,
        event: None,
        result,
    };
    let mut bytes = serde_json::to_vec(&envelope).unwrap();
    bytes.push(b'\n');
    stream.write_all(&bytes).unwrap();
    stream.flush().unwrap();
}

/// 一个可停止的服务线程：listener 非阻塞 + 有界 accept。
struct Server {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn spawn(path: &Path, handler: impl FnOnce(UnixStream) + Send + 'static) -> Server {
        let listener = UnixListener::bind(path).expect("bind stub");
        listener.set_nonblocking(true).unwrap();
        listener
            .set_nonblocking(true)
            .expect("non-blocking listener");
        let stop = Arc::new(AtomicBool::new(false));
        let keep = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if keep.load(Ordering::SeqCst) || Instant::now() >= deadline {
                    break;
                }
                match listener.accept() {
                    Ok((stream, _)) => {
                        stream.set_nonblocking(false).ok();
                        stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
                        stream.set_write_timeout(Some(Duration::from_secs(2))).ok();
                        handler(stream);
                        break;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Server {
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            handle.join().ok();
        }
    }
}

fn socket_path(root: &tempfile::TempDir) -> PathBuf {
    root.path().join("s.sock")
}

#[test]
fn silent_peer_times_out_during_read() {
    let root = short_root("kd-silent");
    let path = socket_path(&root);
    // accept 后既不读也不写：客户端必须在 read 阶段超时。
    let _server = Server::spawn(&path, |stream| {
        std::thread::sleep(Duration::from_millis(500));
        drop(stream);
    });

    let started = Instant::now();
    let error = send_status_with_budget(&path, Duration::from_millis(100))
        .expect_err("silent peer must time out");
    match error {
        ClientError::Timeout { stage } => assert_eq!(stage, ProbeStage::Read),
        other => panic!("expected Timeout(Read), got {other:?}"),
    }
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn drip_feed_cannot_renew_budget() {
    let root = short_root("kd-drip");
    let path = socket_path(&root);
    let _server = Server::spawn(&path, |mut stream| {
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut line)
            .ok();
        // 每 20ms 发一个字节，持续远超客户端预算：若每次读都重置 deadline，
        // 这里会一直读下去而不是在 100ms 后超时。
        let payload = [b'x'; 400];
        for byte in payload {
            if stream.write_all(&[byte]).is_err() {
                break;
            }
            stream.flush().ok();
            std::thread::sleep(Duration::from_millis(20));
        }
    });

    let started = Instant::now();
    let error = send_status_with_budget(&path, Duration::from_millis(100))
        .expect_err("drip feed must not renew the budget");
    assert!(
        matches!(
            error,
            ClientError::Timeout {
                stage: ProbeStage::Read
            }
        ),
        "drip feed must hit the read deadline, got {error:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "budget was renewed: {:?}",
        started.elapsed()
    );
}

#[test]
fn valid_status_and_business_error_are_replies() {
    let root = short_root("kd-valid");
    let path = socket_path(&root);
    {
        let path = path.clone();
        let _server = Server::spawn(&path, |mut stream| {
            reply_status(
                &mut stream,
                Reply::Status {
                    high_water_event_id: 7,
                },
            );
            // 允许同一个服务端依次处理两个连接。
        });
        let reply = send_status_with_budget(&path, Duration::from_secs(2)).expect("status reply");
        assert_eq!(
            reply,
            Reply::Status {
                high_water_event_id: 7
            }
        );
    }

    let root = short_root("kd-error");
    let path = socket_path(&root);
    let _server = Server::spawn(&path, |mut stream| {
        reply_status(
            &mut stream,
            Reply::Error {
                code: "TestOnly".into(),
                message: Some("business failure still proves liveness".into()),
            },
        );
    });
    let reply = send_status_with_budget(&path, Duration::from_secs(2)).expect("error reply");
    assert!(matches!(reply, Reply::Error { ref code, .. } if code == "TestOnly"));
}

#[test]
fn truncated_frame_is_rejected() {
    let root = short_root("kd-trunc");
    let path = socket_path(&root);
    let _server = Server::spawn(&path, |mut stream| {
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut line)
            .ok();
        stream.write_all(b"{\"protocol_version\":3}").unwrap();
        stream.flush().unwrap();
    });
    let error = send_status_with_budget(&path, Duration::from_secs(2)).expect_err("truncated");
    assert!(matches!(error, ClientError::Truncated), "got {error:?}");
}

#[test]
fn malformed_frame_is_rejected() {
    let root = short_root("kd-bad");
    let path = socket_path(&root);
    let _server = Server::spawn(&path, |mut stream| {
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut line)
            .ok();
        stream.write_all(b"not json\n").unwrap();
        stream.flush().unwrap();
    });
    let error = send_status_with_budget(&path, Duration::from_secs(2)).expect_err("malformed");
    assert!(matches!(error, ClientError::Malformed(_)), "got {error:?}");
}

#[test]
fn oversized_stream_is_bounded() {
    let root = short_root("kd-huge");
    let path = socket_path(&root);
    let _server = Server::spawn(&path, |mut stream| {
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut line)
            .ok();
        // 没有换行、超过上限：必须被拒绝，而不是无限累积。
        let chunk = vec![b'x'; 64 * 1024];
        for _ in 0..(superpowers_kanban_ipc::wire::MAX_FRAME_BYTES / chunk.len() + 2) {
            if stream.write_all(&chunk).is_err() {
                break;
            }
        }
        stream.flush().ok();
        std::thread::sleep(Duration::from_millis(100));
    });
    let error = send_status_with_budget(&path, Duration::from_secs(5)).expect_err("oversized");
    assert!(
        matches!(error, ClientError::FrameTooLarge { ref bytes, limit }
            if *bytes > limit && limit == superpowers_kanban_ipc::wire::MAX_FRAME_BYTES),
        "an unterminated over-limit stream must be rejected for size, got {error:?}"
    );
}

#[test]
fn protocol_and_request_id_mismatches_are_rejected() {
    for (name, version, id) in [
        ("version", PROTOCOL_VERSION + 1, "match"),
        ("request", PROTOCOL_VERSION, "other"),
    ] {
        let root = short_root(&format!("kd-{name}"));
        let path = socket_path(&root);
        let _server = Server::spawn(&path, move |mut stream| {
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .ok();
            let request: RequestEnvelope = serde_json::from_str(line.trim()).unwrap();
            let request_id = if id == "match" {
                request.request_id
            } else {
                "other".to_string()
            };
            let envelope = ResponseEnvelope {
                protocol_version: version,
                request_id,
                event_id: None,
                event: None,
                result: Reply::Status {
                    high_water_event_id: 0,
                },
            };
            let mut bytes = serde_json::to_vec(&envelope).unwrap();
            bytes.push(b'\n');
            stream.write_all(&bytes).unwrap();
            stream.flush().unwrap();
        });
        let error = send_status_with_budget(&path, Duration::from_secs(2))
            .expect_err("mismatch must be rejected");
        match name {
            "version" => assert!(
                matches!(error, ClientError::ProtocolVersion { .. }),
                "{error:?}"
            ),
            _ => assert!(
                matches!(error, ClientError::RequestIdMismatch { .. }),
                "{error:?}"
            ),
        }
    }
}

#[test]
fn frame_limit_excludes_newline() {
    // 恰好 MAX_FRAME_BYTES 的 JSON 体 + LF 必须接受；MAX+1 必须拒绝。
    let root = short_root("kd-limit");
    let path = socket_path(&root);
    let _server = Server::spawn(&path, |mut stream| {
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut line)
            .ok();
        let request: RequestEnvelope = serde_json::from_str(line.trim()).unwrap();
        // 构造一个合法的 Error 回复，用 message 撑到精确长度。
        let base = ResponseEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request_id: request.request_id,
            event_id: None,
            event: None,
            result: Reply::Error {
                code: "TestOnly".into(),
                message: Some(String::new()),
            },
        };
        let base_len = serde_json::to_vec(&base).unwrap().len();
        assert!(base_len < superpowers_kanban_ipc::wire::MAX_FRAME_BYTES);
        let pad = superpowers_kanban_ipc::wire::MAX_FRAME_BYTES - base_len;
        let exact = ResponseEnvelope {
            result: Reply::Error {
                code: "TestOnly".into(),
                message: Some("x".repeat(pad)),
            },
            ..base
        };
        let mut bytes = serde_json::to_vec(&exact).unwrap();
        assert_eq!(bytes.len(), superpowers_kanban_ipc::wire::MAX_FRAME_BYTES);
        bytes.push(b'\n');
        stream.write_all(&bytes).unwrap();
        stream.flush().unwrap();
    });
    let reply = send_status_with_budget(&path, Duration::from_secs(5)).expect("max-size frame");
    assert!(matches!(reply, Reply::Error { .. }));

    let root = short_root("kd-limit-over");
    let path = socket_path(&root);
    let _server = Server::spawn(&path, |mut stream| {
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut line)
            .ok();
        let request: RequestEnvelope = serde_json::from_str(line.trim()).unwrap();
        let base = ResponseEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request_id: request.request_id,
            event_id: None,
            event: None,
            result: Reply::Error {
                code: "TestOnly".into(),
                message: Some(String::new()),
            },
        };
        let base_len = serde_json::to_vec(&base).unwrap().len();
        let pad = superpowers_kanban_ipc::wire::MAX_FRAME_BYTES - base_len + 1;
        let over = ResponseEnvelope {
            result: Reply::Error {
                code: "TestOnly".into(),
                message: Some("x".repeat(pad)),
            },
            ..base
        };
        let mut bytes = serde_json::to_vec(&over).unwrap();
        assert_eq!(
            bytes.len(),
            superpowers_kanban_ipc::wire::MAX_FRAME_BYTES + 1
        );
        bytes.push(b'\n');
        stream.write_all(&bytes).ok();
        stream.flush().ok();
    });
    let error = send_status_with_budget(&path, Duration::from_secs(5)).expect_err("over limit");
    assert!(
        matches!(error, ClientError::FrameTooLarge { .. }),
        "{error:?}"
    );
}

#[test]
fn hup_with_complete_frame_is_not_truncated() {
    let root = short_root("kd-hup");
    let path = socket_path(&root);
    let _server = Server::spawn(&path, |mut stream| {
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut line)
            .ok();
        let request: RequestEnvelope = serde_json::from_str(line.trim()).unwrap();
        let envelope = ResponseEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request_id: request.request_id,
            event_id: None,
            event: None,
            result: Reply::Status {
                high_water_event_id: 1,
            },
        };
        let mut bytes = serde_json::to_vec(&envelope).unwrap();
        bytes.push(b'\n');
        stream.write_all(&bytes).unwrap();
        stream.flush().unwrap();
        // 完整帧之后立刻半关闭：已经到手的整帧不能被当成截断。
        stream.shutdown(std::net::Shutdown::Both).ok();
    });
    let reply = send_status_with_budget(&path, Duration::from_secs(2))
        .expect("complete frame survives hup");
    assert_eq!(
        reply,
        Reply::Status {
            high_water_event_id: 1
        }
    );
}

#[test]
fn connection_refused_is_reported() {
    let root = short_root("kd-refused");
    let path = socket_path(&root);
    let error = send_status_with_budget(&path, Duration::from_secs(1)).expect_err("no listener");
    assert!(matches!(error, ClientError::Io(_)), "{error:?}");
}

/// 防止测试里误用：读端必须在有限时间内结束。
#[allow(dead_code)]
fn read_to_end_bounded(stream: &mut UnixStream) -> Vec<u8> {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).ok();
    bytes
}
