//! Boundary tests for the control channel's framing.
//!
//! The control channel is the one path every frontend and the guardian share, so
//! its decoder is the first thing a hostile or buggy peer touches. Every case
//! here is a frame the daemon must refuse *before* it allocates the declared
//! body: an over-long length, a lying length, a foreign protocol version, and a
//! body that is not UTF-8.

use std::time::Duration;

use yi_agent_lifecycle::codec::{decode, encode};
use yi_agent_lifecycle::deadline::{Clock, Deadline};
use yi_agent_lifecycle::types::{ControlRequest, ControlResponse, RuntimeMode};
use yi_agent_lifecycle::{CONTROL_VERSION, MAX_CONTROL_FRAME};

#[test]
fn oversized_frame_is_rejected_before_body_allocation() {
    let input = ((16 * 1024) as u32).to_be_bytes();
    let error = decode::<serde_json::Value>(&input).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn frame_at_the_exact_ceiling_is_accepted() {
    // MAX_CONTROL_FRAME counts the 4-byte header, so the largest legal body is
    // MAX_CONTROL_FRAME - 4. Measure the envelope overhead from an empty string
    // payload, then pad to exactly the ceiling and confirm the boundary itself
    // is not off by one in either direction.
    let empty = String::new();
    let envelope = yi_agent_lifecycle::codec::test_envelope(&empty);
    let overhead = envelope.len();
    let target = MAX_CONTROL_FRAME - 4;
    let padding = target - overhead;
    let payload = "a".repeat(padding);
    let body = yi_agent_lifecycle::codec::test_envelope(&payload);
    assert_eq!(body.len(), target);

    let mut frame = (body.len() as u32).to_be_bytes().to_vec();
    frame.extend_from_slice(body.as_bytes());
    let decoded: String = decode(&frame).unwrap();
    assert_eq!(decoded.len(), padding);
}

#[test]
fn frame_one_byte_over_the_ceiling_is_rejected() {
    let empty = String::new();
    let overhead = yi_agent_lifecycle::codec::test_envelope(&empty).len();
    let payload = "a".repeat(MAX_CONTROL_FRAME - 4 - overhead + 1);
    let body = yi_agent_lifecycle::codec::test_envelope(&payload);
    let mut frame = (body.len() as u32).to_be_bytes().to_vec();
    frame.extend_from_slice(body.as_bytes());
    let error = decode::<String>(&frame).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn truncated_body_is_rejected() {
    let mut frame = encode(&ControlRequest::Identity).unwrap();
    frame.truncate(frame.len() - 3);
    let error = decode::<ControlRequest>(&frame).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
}

#[test]
fn header_longer_than_supplied_body_is_rejected() {
    let mut frame = encode(&ControlRequest::Identity).unwrap();
    frame.truncate(4);
    let error = decode::<ControlRequest>(&frame).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
}

#[test]
fn short_header_is_rejected() {
    let error = decode::<ControlRequest>(&[0, 0]).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
}

#[test]
fn unknown_control_version_is_rejected() {
    let body = format!(
        "{{\"control_version\":{},\"payload\":{{\"Identity\":null}}}}",
        CONTROL_VERSION + 1
    );
    let mut frame = (body.len() as u32).to_be_bytes().to_vec();
    frame.extend_from_slice(body.as_bytes());
    let error = decode::<ControlRequest>(&frame).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn invalid_utf8_is_rejected() {
    let body = [0xff, 0xfe, 0xfd];
    let mut frame = (body.len() as u32).to_be_bytes().to_vec();
    frame.extend_from_slice(&body);
    let error = decode::<ControlRequest>(&frame).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn request_and_response_round_trip() {
    let request = ControlRequest::Register {
        client_run_id: "app-1".into(),
        kind: yi_agent_lifecycle::types::ClientKind::App,
        mode: yi_agent_lifecycle::types::LeaseMode::Business,
    };
    let wire = encode(&request).unwrap();
    assert_eq!(decode::<ControlRequest>(&wire).unwrap(), request);

    let response = ControlResponse::Unsupported {
        detail: "no such operation".into(),
    };
    let wire = encode(&response).unwrap();
    assert_eq!(decode::<ControlResponse>(&wire).unwrap(), response);
}

#[test]
fn encode_refuses_a_body_that_cannot_fit() {
    // A payload bigger than the ceiling is a programming error, not a peer
    // error: the caller must learn about it before writing a header that lies.
    let big = ControlResponse::Rejected {
        reason: "x".repeat(MAX_CONTROL_FRAME),
        code: yi_agent_lifecycle::types::ControlErrorCode::InvalidRequest,
    };
    let error = encode(&big).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn runtime_mode_is_snake_case_on_the_wire() {
    let json = serde_json::to_string(&RuntimeMode::LegacyEmbedded).unwrap();
    assert_eq!(json, "\"legacy_embedded\"");
}

struct FakeClock(std::sync::Mutex<Duration>);

impl FakeClock {
    fn new() -> Self {
        Self(std::sync::Mutex::new(Duration::ZERO))
    }
    fn advance(&self, by: Duration) {
        *self.0.lock().unwrap() += by;
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Duration {
        *self.0.lock().unwrap()
    }
}

#[test]
fn deadline_remaining_shrinks_without_wall_clock() {
    let clock = std::sync::Arc::new(FakeClock::new());
    let deadline = Deadline::with_clock(Duration::from_secs(10), clock.clone());
    assert_eq!(deadline.remaining(), Duration::from_secs(10));
    clock.advance(Duration::from_secs(4));
    assert_eq!(deadline.remaining(), Duration::from_secs(6));
    clock.advance(Duration::from_secs(60));
    assert_eq!(deadline.remaining(), Duration::ZERO);
}

#[test]
fn clamp_never_exceeds_the_remaining_budget() {
    let clock = std::sync::Arc::new(FakeClock::new());
    let deadline = Deadline::with_clock(Duration::from_secs(3), clock.clone());
    assert_eq!(
        deadline.clamp(Duration::from_secs(2)),
        Duration::from_secs(2)
    );
    assert_eq!(
        deadline.clamp(Duration::from_secs(9)),
        Duration::from_secs(3)
    );
    clock.advance(Duration::from_secs(10));
    assert_eq!(deadline.clamp(Duration::from_secs(9)), Duration::ZERO);
}

#[test]
fn zero_budget_is_expired_not_immediately_usable() {
    let deadline = Deadline::after(Duration::ZERO);
    assert_eq!(deadline.remaining(), Duration::ZERO);
    assert!(deadline.is_expired());
}
