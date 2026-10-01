use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::wire::{
    Command, MAX_FRAME_BYTES, PROTOCOL_VERSION, Reply, RequestEnvelope, ResponseEnvelope,
};

#[derive(Debug)]
pub enum ClientError {
    Io(std::io::Error),
    Malformed(String),
    ProtocolVersion { found: u32, expected: u32 },
    RequestIdMismatch { found: String, expected: String },
    FrameTooLarge { bytes: usize, limit: usize },
    Truncated,
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Io(error) => write!(f, "ipc io error: {error}"),
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

/// The socket the daemon listens on for a given runtime directory.
///
/// 复刻宿主的回退规则（`yi-agent-` 前缀、哈希 `runtime_dir`）：深路径下宿主的
/// daemon 会把 socket 挪到临时目录，插件必须落到**同一处**才连得上。
pub fn socket_path(runtime_dir: &Path) -> Result<PathBuf, crate::socket::SocketPathError> {
    crate::socket::daemon_socket_for(runtime_dir)
}

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

fn next_request_id() -> String {
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

    #[test]
    fn the_socket_lives_beside_its_runtime_directory() {
        assert_eq!(
            socket_path(std::path::Path::new("/project/.yi-agent/runtime")).expect("short path stays"),
            std::path::PathBuf::from("/project/.yi-agent/runtime/runtime.sock")
        );
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
            r#"{"protocol_version":1,"request_id":"7","command":{"type":"ListTaskSummaries","session_id":null,"active_only":false}}"#
        );
    }

    #[test]
    fn a_matching_response_decodes_to_its_reply() {
        let line = br#"{"protocol_version":1,"request_id":"7","result":{"type":"AutonomousSessionCreated","session_id":"s","root_task_id":"t"}}"#;
        let reply = decode_response(line, "7").unwrap();
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
        let line = br#"{"protocol_version":1,"request_id":"8","result":{"type":"Status","high_water_event_id":0}}"#;
        let error = decode_response(line, "7").unwrap_err();
        assert!(
            matches!(error, ClientError::RequestIdMismatch { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn a_different_protocol_version_is_rejected() {
        let line = br#"{"protocol_version":2,"request_id":"7","result":{"type":"Status","high_water_event_id":0}}"#;
        let error = decode_response(line, "7").unwrap_err();
        assert!(
            matches!(
                error,
                ClientError::ProtocolVersion {
                    found: 2,
                    expected: 1
                }
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
