use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;
use yi_agent_core::subagent::task::{RootSessionId, TaskId};
use yi_agent_store::ipc::{
    Daemon, IpcRequest, IpcResponse, RequestEnvelope, send_request, subscribe,
};
use yi_agent_store::repository::{RuntimeEvent, RuntimeRepository};

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
fn subscription_reconnect_replays_strictly_ordered_events_without_duplicates() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let daemon = Daemon::start(directory.path().join("runtime"), &database).unwrap();
    let mut repository = RuntimeRepository::open(&database).unwrap();
    let root = RootSessionId::new();
    let task = TaskId::new();
    repository.create_task(&task, &root, "queued").unwrap();
    repository
        .append_event(&task, RuntimeEvent::TaskQueued)
        .unwrap();

    let mut initial = subscribe(daemon.socket_path(), 0).unwrap();
    let IpcResponse::Subscription(snapshot) = initial.next_response().unwrap() else {
        panic!("expected initial subscription snapshot");
    };
    assert!(snapshot.events.is_empty());
    repository
        .transition_task(&task, "running", RuntimeEvent::TaskStarted)
        .unwrap();
    let IpcResponse::Event(initial_event) = initial.next_response().unwrap() else {
        panic!("expected initial live event");
    };
    let saved_event_id = initial_event.event_id;
    drop(initial);

    repository
        .transition_task(&task, "completed", RuntimeEvent::TaskCompleted)
        .unwrap();
    repository
        .append_event(&task, RuntimeEvent::TaskProgress)
        .unwrap();

    let mut resumed = subscribe(daemon.socket_path(), saved_event_id).unwrap();
    let IpcResponse::Subscription(snapshot) = resumed.next_response().unwrap() else {
        panic!("expected resumed subscription snapshot");
    };
    let replay_ids = snapshot
        .events
        .iter()
        .map(|event| event.event_id)
        .collect::<Vec<_>>();
    assert_eq!(replay_ids.len(), 2);
    assert!(replay_ids.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(replay_ids.iter().all(|id| *id > saved_event_id));
    assert_eq!(
        replay_ids
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        replay_ids.len()
    );

    repository
        .append_event(&task, RuntimeEvent::TaskQueued)
        .unwrap();
    let IpcResponse::Event(live) = resumed.next_response().unwrap() else {
        panic!("expected live event after reconnect");
    };
    assert!(live.event_id > *replay_ids.last().unwrap());
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
