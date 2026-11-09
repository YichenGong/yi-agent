//! A probe that decides "is a daemon already here?" must be bounded and read-only.
//!
//! Three ways this can go wrong, all of which show up as a *start* decision made
//! from bad evidence: the peer never answers and the probe hangs forever, the peer
//! answers half a frame and the probe reads the prefix as a reply, or the peer is
//! not there at all and the probe reports a transport error the caller cannot
//! distinguish from "wedged". Each test drives one of those.

use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use yi_agent_lifecycle::deadline::Deadline;
use yi_agent_store::ipc::{IpcResponse, PROTOCOL_VERSION, send_status_until};

fn socket_path(directory: &TempDir) -> PathBuf {
    directory.path().join("business.sock")
}

fn status_envelope(request_id: &str) -> String {
    // The probe sends one `Status`; the answer only has to be well-formed enough to
    // prove the envelope checks ran, so an empty-ish payload is built from the
    // real response type rather than hand-written JSON.
    let response = serde_json::to_value(IpcResponse::Status {
        high_water_event_id: 42,
    })
    .expect("Status serializes");
    serde_json::json!({
        "protocol_version": PROTOCOL_VERSION,
        "request_id": request_id,
        "result": response,
    })
    .to_string()
}

/// A listener that accepts and then says nothing must not hold the probe.
#[test]
fn silent_peer_returns_within_the_budget() {
    let directory = TempDir::new().unwrap();
    let path = socket_path(&directory);
    let listener = UnixListener::bind(&path).unwrap();
    let held = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        // Hold the connection open without answering, but bounded so a probe that
        // wrongly waits forever fails the test rather than hanging the suite.
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        thread::sleep(Duration::from_secs(2));
        drop(stream);
    });

    let started = Instant::now();
    let outcome = send_status_until(&path, &Deadline::after(Duration::from_millis(300)));
    let elapsed = started.elapsed();

    assert!(outcome.is_err(), "a silent peer cannot count as an answer");
    assert!(
        elapsed < Duration::from_secs(5),
        "the probe must return on its budget, took {elapsed:?}"
    );
    // Not joined: the peer is deliberately unresponsive, so waiting for it would
    // test the fixture rather than the probe.
    drop(held);
}

/// Half a frame is a broken frame, not a small one.
#[test]
fn partial_frame_is_not_read_as_a_reply() {
    let directory = TempDir::new().unwrap();
    let path = socket_path(&directory);
    let listener = UnixListener::bind(&path).unwrap();
    let held = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut buffer = [0u8; 1024];
        let _ = stream.read(&mut buffer);
        let full = status_envelope("unused");
        let half = &full.as_bytes()[..full.len() / 2];
        let _ = stream.write_all(half);
        let _ = stream.flush();
        thread::sleep(Duration::from_secs(1));
    });

    let outcome = send_status_until(&path, &Deadline::after(Duration::from_millis(500)));

    assert!(
        outcome.is_err(),
        "a truncated frame must not parse as a successful Status"
    );
    drop(held);
}

/// An envelope addressed to a different request is not an answer to this one.
#[test]
fn mismatched_envelope_identity_is_rejected() {
    let directory = TempDir::new().unwrap();
    let path = socket_path(&directory);
    let listener = UnixListener::bind(&path).unwrap();
    let worker = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut buffer = [0u8; 4096];
        let _ = stream.read(&mut buffer);
        let reply = status_envelope("some-other-request");
        let _ = stream.write_all(reply.as_bytes());
        let _ = stream.write_all(b"\n");
        let _ = stream.flush();
    });

    let outcome = send_status_until(&path, &Deadline::after(Duration::from_secs(2)));

    assert!(
        outcome.is_err(),
        "a reply for another request must not be accepted as this probe's Status"
    );
    worker.join().unwrap();
}

/// The happy path: a real Status comes back as a Status.
#[test]
fn valid_status_is_answered() {
    let directory = TempDir::new().unwrap();
    let path = socket_path(&directory);
    let listener = UnixListener::bind(&path).unwrap();
    let worker = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut buffer = [0u8; 4096];
        let read = stream.read(&mut buffer).unwrap();
        let request: serde_json::Value = serde_json::from_slice(&buffer[..read]).unwrap();
        let request_id = request["request_id"].as_str().unwrap().to_string();
        let reply = status_envelope(&request_id);
        stream.write_all(reply.as_bytes()).unwrap();
        stream.write_all(b"\n").unwrap();
        stream.flush().unwrap();
    });

    let outcome = send_status_until(&path, &Deadline::after(Duration::from_secs(2)))
        .expect("a well-formed Status must be accepted");

    assert!(
        matches!(
            outcome,
            IpcResponse::Status {
                high_water_event_id: 42
            }
        ),
        "expected the daemon's Status, got {outcome:?}"
    );
    worker.join().unwrap();
}

/// A peer that drips one byte at a time must still be cut off at the budget.
///
/// This is the reason the probe polls the *remaining* budget instead of setting a
/// socket read timeout. A per-syscall timeout is satisfied by every single byte, so
/// a peer sending one byte just under the timeout can hold the probe for as long as
/// it likes -- and the caller reads the hang as "still starting".
#[test]
fn a_dripping_peer_cannot_extend_the_budget() {
    let directory = TempDir::new().unwrap();
    let path = socket_path(&directory);
    let listener = UnixListener::bind(&path).unwrap();
    let worker = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut buffer = [0u8; 4096];
        let _ = stream.read(&mut buffer);
        // 40 bytes at 50ms each: 2s of wall clock, far beyond a 300ms budget. Every
        // individual byte arrives well inside any per-syscall timeout.
        for _ in 0..40 {
            if stream.write_all(b"{").is_err() {
                return;
            }
            let _ = stream.flush();
            thread::sleep(Duration::from_millis(50));
        }
    });

    let started = Instant::now();
    let outcome = send_status_until(&path, &Deadline::after(Duration::from_millis(300)));
    let elapsed = started.elapsed();

    assert!(outcome.is_err(), "an unfinished frame is not a Status");
    assert!(
        elapsed < Duration::from_millis(1200),
        "the budget must bound the whole exchange, took {elapsed:?}"
    );
    drop(worker);
}

/// Nothing listening is a transport error the caller can classify, not a hang.
#[test]
fn missing_socket_fails_fast() {
    let directory = TempDir::new().unwrap();
    let path = socket_path(&directory);

    let started = Instant::now();
    let outcome = send_status_until(&path, &Deadline::after(Duration::from_secs(2)));
    let elapsed = started.elapsed();

    assert!(outcome.is_err(), "a missing socket is not a live daemon");
    assert!(
        elapsed < Duration::from_secs(2),
        "a missing socket must fail immediately, took {elapsed:?}"
    );
}
