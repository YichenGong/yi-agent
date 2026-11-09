use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::wire::{
    Command, MAX_FRAME_BYTES, PROTOCOL_VERSION, Reply, RequestEnvelope, ResponseEnvelope,
};

/// 探活卡在哪个阶段。超时诊断必须说清「连不上」还是「连上了但不说话」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeStage {
    Connect,
    Write,
    Read,
}

impl std::fmt::Display for ProbeStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProbeStage::Connect => write!(f, "connect"),
            ProbeStage::Write => write!(f, "write"),
            ProbeStage::Read => write!(f, "read"),
        }
    }
}

#[derive(Debug)]
pub enum ClientError {
    Io(std::io::Error),
    /// 共享预算耗尽；`stage` 说明卡在哪一步。
    Timeout {
        stage: ProbeStage,
    },
    Malformed(String),
    ProtocolVersion {
        found: u32,
        expected: u32,
    },
    RequestIdMismatch {
        found: String,
        expected: String,
    },
    FrameTooLarge {
        bytes: usize,
        limit: usize,
    },
    Truncated,
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Io(error) => write!(f, "ipc io error: {error}"),
            ClientError::Timeout { stage } => {
                write!(f, "ipc probe timeout during {stage}")
            }
            ClientError::Malformed(message) => write!(f, "malformed ipc frame: {message}"),
            ClientError::ProtocolVersion { found, expected } => write!(
                f,
                "ipc protocol version mismatch: daemon sent {found}, plugin speaks {expected}"
            ),
            ClientError::RequestIdMismatch { found, expected } => {
                write!(
                    f,
                    "ipc response is for request {found}, expected {expected}"
                )
            }
            ClientError::FrameTooLarge { bytes, limit } => {
                write!(f, "ipc frame is {bytes} bytes, over the {limit}-byte limit")
            }
            ClientError::Truncated => write!(f, "ipc frame ended without a newline"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<std::io::Error> for ClientError {
    fn from(error: std::io::Error) -> Self {
        ClientError::Io(error)
    }
}

/// The socket the business daemon listens on for a given runtime directory.
///
/// 宿主现在的布局是 `<instance_home>/sockets/<canonical runtime 的 SHA-256 前
/// 16 hex>/runtime.sock`（见 [`crate::daemon_socket`]），**不再**把 socket 放在
/// runtime 目录旁、也**不再**回退到 TMPDIR。旧注释描述的直通/TMPDIR 规则会让
/// 排查者按错误地址去 lsof，所以一并删除。
///
/// 查询通道（`plugin-` 前缀、TMPDIR 回退）仍是另一套规则，见 [`crate::socket`]。
pub fn socket_path(runtime_dir: &Path) -> Result<PathBuf, crate::daemon_socket::DaemonSocketError> {
    crate::daemon_socket::daemon_socket_for(runtime_dir)
}

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) fn next_request_id() -> String {
    NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed).to_string()
}

/// Serializes one request as a single newline-terminated frame.
pub fn encode_request(request_id: &str, command: Command) -> Result<Vec<u8>, ClientError> {
    let envelope = RequestEnvelope {
        protocol_version: PROTOCOL_VERSION,
        request_id: request_id.to_owned(),
        command,
    };
    let mut frame =
        serde_json::to_vec(&envelope).map_err(|error| ClientError::Malformed(error.to_string()))?;
    if frame.len() > MAX_FRAME_BYTES {
        return Err(ClientError::FrameTooLarge {
            bytes: frame.len(),
            limit: MAX_FRAME_BYTES,
        });
    }
    frame.push(b'\n');
    Ok(frame)
}

/// Validates one response frame and returns its reply.
pub fn decode_response(line: &[u8], expected_request_id: &str) -> Result<Reply, ClientError> {
    let body = line.strip_suffix(b"\n").unwrap_or(line);
    if body.len() > MAX_FRAME_BYTES {
        return Err(ClientError::FrameTooLarge {
            bytes: body.len(),
            limit: MAX_FRAME_BYTES,
        });
    }
    let envelope: ResponseEnvelope =
        serde_json::from_slice(body).map_err(|error| ClientError::Malformed(error.to_string()))?;
    if envelope.protocol_version != PROTOCOL_VERSION {
        return Err(ClientError::ProtocolVersion {
            found: envelope.protocol_version,
            expected: PROTOCOL_VERSION,
        });
    }
    if envelope.request_id != expected_request_id {
        return Err(ClientError::RequestIdMismatch {
            found: envelope.request_id,
            expected: expected_request_id.to_owned(),
        });
    }
    Ok(envelope.result)
}

/// `Status` 的唯一有界入口：连接、写、读共享一个预算。
///
/// 只允许 Status。批量/会话创建走原有的 [`send`]（无预算），它们的返回语义
/// 不在这里改变——本次修复只针对探活无限阻塞。
pub fn send_status_with_budget(
    socket: &Path,
    budget: std::time::Duration,
) -> Result<Reply, ClientError> {
    let started = std::time::Instant::now();
    let deadline = started.checked_add(budget).ok_or_else(|| {
        ClientError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "probe budget overflow",
        ))
    })?;
    let request_id = next_request_id();
    let request = encode_request(&request_id, Command::Status)?;
    let response = crate::status_io::exchange(socket, &request, deadline)?;
    decode_response(&response, &request_id)
}

/// Sends one request and returns the reply. One connection per request, which
/// matches how the daemon serves non-subscription traffic.
pub fn send(socket: &Path, command: Command) -> Result<Reply, ClientError> {
    let request_id = next_request_id();
    let frame = encode_request(&request_id, command)?;
    let mut stream = UnixStream::connect(socket)?;
    stream.write_all(&frame)?;
    stream.flush()?;
    let mut reader = BufReader::new(stream);
    let mut line = Vec::new();
    let read = reader.read_until(b'\n', &mut line)?;
    if read == 0 {
        return Err(ClientError::Truncated);
    }
    if !line.ends_with(b"\n") {
        return Err(ClientError::Truncated);
    }
    decode_response(&line, &request_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::Command;

    /// 旧断言（socket 就在 runtime 目录旁）会把宿主的新布局永久锁死，必须替换。
    /// 用纯入口而不是读继承的 HOME，断言才是确定的。
    #[test]
    fn the_socket_lives_in_the_instance_socket_directory() {
        use crate::daemon_socket::daemon_socket_with_home;
        let resolved = daemon_socket_with_home(
            std::path::Path::new("/project/.yi-agent/runtime"),
            std::path::Path::new("/instance"),
        )
        .expect("short path resolves");
        assert!(resolved.starts_with("/instance/sockets"), "{resolved:?}");
        assert_eq!(resolved.file_name().unwrap(), "runtime.sock");
        assert_eq!(resolved.components().count(), 5, "{resolved:?}");
    }

    #[test]
    fn an_encoded_request_is_one_newline_terminated_json_line() {
        let frame = encode_request(
            "7",
            Command::ListTaskSummaries {
                session_id: None,
                active_only: false,
            },
        )
        .unwrap();
        assert!(frame.ends_with(b"\n"));
        assert_eq!(frame.iter().filter(|byte| **byte == b'\n').count(), 1);
        let text = std::str::from_utf8(&frame[..frame.len() - 1]).unwrap();
        assert_eq!(
            text,
            format!(
                r#"{{"protocol_version":{PROTOCOL_VERSION},"request_id":"7","command":{{"type":"ListTaskSummaries","session_id":null,"active_only":false}}}}"#
            )
        );
    }

    #[test]
    fn a_matching_response_decodes_to_its_reply() {
        let line = format!(r#"{{"protocol_version":{PROTOCOL_VERSION},"request_id":"7","result":{{"type":"AutonomousSessionCreated","session_id":"s","root_task_id":"t"}}}}"#).into_bytes();
        let reply = decode_response(&line, "7").unwrap();
        assert_eq!(
            reply,
            crate::wire::Reply::AutonomousSessionCreated {
                session_id: "s".into(),
                root_task_id: "t".into(),
            }
        );
    }

    #[test]
    fn a_response_for_another_request_is_rejected() {
        let line = format!(r#"{{"protocol_version":{PROTOCOL_VERSION},"request_id":"8","result":{{"type":"Status","high_water_event_id":0}}}}"#).into_bytes();
        let error = decode_response(&line, "7").unwrap_err();
        assert!(
            matches!(error, ClientError::RequestIdMismatch { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn a_different_protocol_version_is_rejected() {
        // 用一个**确定不等于**当前版本的版本号：写死 2 的话，等版本升到 2 这条测试就失效了。
        let other = PROTOCOL_VERSION + 1;
        let line = format!(r#"{{"protocol_version":{other},"request_id":"7","result":{{"type":"Status","high_water_event_id":0}}}}"#).into_bytes();
        let error = decode_response(&line, "7").unwrap_err();
        assert!(
            matches!(
                error,
                ClientError::ProtocolVersion {
                    found,
                    expected: PROTOCOL_VERSION
                } if found == other
            ),
            "{error:?}"
        );
    }

    #[test]
    fn an_oversized_frame_is_rejected_before_parsing() {
        let line = vec![b'x'; crate::wire::MAX_FRAME_BYTES + 1];
        let error = decode_response(&line, "7").unwrap_err();
        assert!(
            matches!(error, ClientError::FrameTooLarge { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn malformed_json_is_rejected_with_a_readable_error() {
        let error = decode_response(b"not json", "7").unwrap_err();
        assert!(matches!(error, ClientError::Malformed(_)), "{error:?}");
    }
}
