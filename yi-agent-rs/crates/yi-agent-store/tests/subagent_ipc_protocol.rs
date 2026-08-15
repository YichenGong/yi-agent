use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;
use yi_agent_store::ipc::{Daemon, IpcRequest, RequestEnvelope, send_request};

fn read_response(stream: UnixStream) -> Option<Value> {
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let mut line = String::new();
    let read = BufReader::new(stream).read_line(&mut line).unwrap();
    (read > 0).then(|| serde_json::from_str(&line).unwrap())
}

fn send_in_chunks(socket: &Path, bytes: &[u8], chunk_size: usize) -> Value {
    let mut stream = UnixStream::connect(socket).unwrap();
    for chunk in bytes.chunks(chunk_size) {
        stream.write_all(chunk).unwrap();
    }
    stream.write_all(b"\n").unwrap();
    stream.flush().unwrap();
    read_response(stream).expect("complete frame must receive a response")
}

fn assert_healthy(socket: &Path) {
    assert!(matches!(
        send_request(socket, IpcRequest::Status).unwrap(),
        yi_agent_store::ipc::IpcResponse::Status { .. }
    ));
}

#[test]
fn fragmented_status_frames_preserve_request_correlation() {
    let directory = TempDir::new().unwrap();
    let daemon = Daemon::start(
        directory.path().join("runtime"),
        &directory.path().join("runtime.sqlite"),
    )
    .unwrap();
    let envelope = RequestEnvelope {
        protocol_version: 1,
        request_id: "fragmented-status".into(),
        command: IpcRequest::Status,
    };
    let frame = serde_json::to_vec(&envelope).unwrap();

    for size in 1..=frame.len() {
        let response = send_in_chunks(daemon.socket_path(), &frame, size);
        assert_eq!(response["protocol_version"], json!(1));
        assert_eq!(response["request_id"], json!("fragmented-status"));
        assert!(
            response["result"].is_object(),
            "status response must contain a result object: {response}"
        );
    }
}

#[test]
fn malformed_and_oversized_clients_do_not_stop_healthy_requests() {
    let directory = TempDir::new().unwrap();
    let daemon = Daemon::start(
        directory.path().join("runtime"),
        &directory.path().join("runtime.sqlite"),
    )
    .unwrap();
    for frame in [
        b"{\"protocol_version\":1".as_slice(),
        b"{\"protocol_version\":999,\"request_id\":\"bad-version\",\"command\":\"status\"}"
            .as_slice(),
        b"{\"protocol_version\":1,\"request_id\":\"unknown\",\"command\":\"unknown\"}".as_slice(),
    ] {
        let mut stream = UnixStream::connect(daemon.socket_path()).unwrap();
        stream.write_all(frame).unwrap();
        stream.write_all(b"\n").unwrap();
        stream.flush().unwrap();
        let _ = read_response(stream);
        assert_healthy(daemon.socket_path());
    }

    let mut stream = UnixStream::connect(daemon.socket_path()).unwrap();
    stream.write_all(&vec![b'x'; 1024 * 1024 + 1]).unwrap();
    stream.write_all(b"\n").unwrap();
    stream.flush().unwrap();
    let _ = read_response(stream);
    assert_healthy(daemon.socket_path());
}
