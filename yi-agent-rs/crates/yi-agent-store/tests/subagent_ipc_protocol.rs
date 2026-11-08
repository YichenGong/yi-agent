use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;
use yi_agent_core::subagent::task::{RootSessionId, TaskId};
use yi_agent_store::ipc::{
    Daemon, IpcRequest, IpcResponse, PROTOCOL_VERSION, RequestEnvelope, send_request, subscribe,
};
use yi_agent_store::repository::{RuntimeEvent, RuntimeRepository};

/// 读一帧响应。
///
/// 超时是**失败判据**而不是节流器：这里要区分"daemon 真没回"与"daemon 只是
/// 排队慢"。原先 500ms 在满载（全量并发跑测试）下会把后者判成前者——
/// `fragmented_status_frames_preserve_request_correlation` 单跑要发 75 轮、
/// 2850 次写，任一往返稍慢就撞线，表现为偶发 flaky。放宽到 30s 后仍能抓住
/// 真正挂死的 daemon（那时读完就是超时），但不再因调度抖动误杀。
fn read_response(stream: UnixStream) -> Option<Value> {
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
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
        directory.path().join("runtime.sqlite"),
    )
    .unwrap();
    let envelope = RequestEnvelope {
        protocol_version: PROTOCOL_VERSION,
        request_id: "fragmented-status".into(),
        command: IpcRequest::Status,
    };
    let frame = serde_json::to_vec(&envelope).unwrap();

    for size in 1..=frame.len() {
        let response = send_in_chunks(daemon.socket_path(), &frame, size);
        assert_eq!(response["protocol_version"], json!(PROTOCOL_VERSION));
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
        directory.path().join("runtime.sqlite"),
    )
    .unwrap();
    for frame in [
        b"{\"protocol_version\":1".as_slice(),
        b"{\"protocol_version\":9999,\"request_id\":\"bad-version\",\"command\":\"status\"}"
            .as_slice(),
        b"{\"protocol_version\":2,\"request_id\":\"unknown\",\"command\":\"unknown\"}".as_slice(),
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

/// A new daemon advertises the live-apply capability on `Status`.
///
/// The field is additive and optional: a client that has never heard of it
/// reads the same frame as before, and an old daemon's frame (no field) reads
/// as `None`, which is exactly the "old capability, degrade" signal. Both
/// directions are asserted here because only the pair makes the claim
/// meaningful -- serializing `Some` alone would pass even if parsing a missing
/// field errored out.
#[test]
fn status_reports_the_live_apply_capability_and_tolerates_old_daemons() {
    let dir = TempDir::new().unwrap();
    let daemon = Daemon::start(dir.path().join("runtime"), dir.path().join("db")).unwrap();

    let response = send_request(daemon.socket_path(), IpcRequest::Status).unwrap();
    let yi_agent_store::ipc::IpcResponse::Status {
        live_apply_version, ..
    } = response
    else {
        panic!("Status must answer with a Status frame, got {response:?}");
    };
    assert_eq!(
        live_apply_version,
        Some(yi_agent_store::ipc::LIVE_APPLY_VERSION),
        "a daemon that applies settings live must say so"
    );

    // The same frame on the wire carries the field ...
    let frame = serde_json::to_value(yi_agent_store::ipc::IpcResponse::Status {
        high_water_event_id: 7,
        live_apply_version: Some(yi_agent_store::ipc::LIVE_APPLY_VERSION),
    })
    .unwrap();
    assert_eq!(
        frame["live_apply_version"],
        json!(yi_agent_store::ipc::LIVE_APPLY_VERSION)
    );

    // ... and an old daemon's frame, which has no such field, must still parse.
    let old = json!({ "type": "Status", "high_water_event_id": 7 });
    let parsed: IpcResponse = serde_json::from_value(old).unwrap();
    assert_eq!(
        parsed,
        IpcResponse::Status {
            high_water_event_id: 7,
            live_apply_version: None,
        },
        "an old daemon must read as 'no live apply', not as a parse error"
    );
}
