//! The controlled handover of the control plane across `exec`.
//!
//! The unit tests in `lifecycle::handover` cover the format. This file covers the
//! decisions a *receiver* makes, end to end, on a real descriptor: what it accepts,
//! what it refuses, and what it must never extend.
//!
//! The properties under test are all about not trusting the carrier:
//!
//! - a state for another runtime or another target is refused, not reinterpreted;
//! - a lease the old guardian released is not resurrected;
//! - a consumed confirmation token cannot be replayed from the carried identity;
//! - a state over the limit is refused *before* anything stops.

mod support;

use std::os::fd::AsFd;
use std::path::PathBuf;

use yi_agent_lifecycle::types::{
    ArtifactIdentity, ClientGrant, LeaseMode, OperationStatus, RuntimeScope, SwitchPhase,
};
use yi_agent_supervisors::lifecycle::handover::{
    self, DescriptorRole, HANDOVER_VERSION, HandoverRefusal, HandoverState, MAX_HANDOVER_BYTES,
    PreservedDescriptor, ReleasedIdentity, RemainingLease,
};

fn scope(key: &str) -> RuntimeScope {
    RuntimeScope {
        key: key.into(),
        runtime_dir: PathBuf::from(key),
        project_key: "/project".into(),
        instance_home: PathBuf::from(key),
    }
}

fn target(build: &str) -> ArtifactIdentity {
    ArtifactIdentity {
        build_id: build.into(),
        display_version: "0.2.0".into(),
        executable: "/usr/bin/true".into(),
        sha256: "aa".into(),
        schema_min: 1,
        schema_max: 16,
        handover_version: 1,
        business_protocol_min: 1,
        business_protocol_max: 1,
        capabilities: vec![],
    }
}

fn grant(client: &str, epoch: u64) -> ClientGrant {
    ClientGrant {
        client_run_id: client.into(),
        mode: LeaseMode::Business,
        epoch,
        credential: "do-not-carry-this".into(),
    }
}

fn state() -> HandoverState {
    HandoverState {
        version: HANDOVER_VERSION,
        scope_key: "/project/runtime".into(),
        operation: OperationStatus {
            operation_id: "op-1".into(),
            phase: SwitchPhase::Starting,
            detail: None,
            ready_identity: None,
        },
        consumed_tokens: vec!["token-identity".into()],
        captured_monotonic_ns: handover::cross_process_monotonic_ns(),
        remaining_leases_ms: vec![RemainingLease {
            grant: grant("tui", 1),
            remaining_ms: 20_000,
        }],
        remaining_start_budget_ms: 25_000,
        target: target("next"),
        guardian_revision: 9,
        released: Vec::new(),
        preserved: vec![
            PreservedDescriptor {
                role: DescriptorRole::ControlListener,
                client_run_id: None,
                epoch: None,
            },
            PreservedDescriptor {
                role: DescriptorRole::RegisteredConnection,
                client_run_id: Some("tui".into()),
                epoch: Some(1),
            },
        ],
    }
}

/// A full carrier round trip that the receiver accepts for the right runtime.
#[test]
fn a_matching_state_is_accepted_for_its_own_runtime() {
    let original = state();
    let fd = handover::encode_state(&original).unwrap();
    let decoded = handover::decode_state(fd.as_fd()).unwrap();
    handover::validate_state(&decoded, &scope("/project/runtime"), &target("next")).unwrap();
    assert_eq!(decoded, original);
}

#[test]
fn a_state_for_a_different_runtime_is_refused() {
    let fd = handover::encode_state(&state()).unwrap();
    let decoded = handover::decode_state(fd.as_fd()).unwrap();
    let refusal =
        handover::validate_state(&decoded, &scope("/other/runtime"), &target("next")).unwrap_err();
    assert!(matches!(refusal, HandoverRefusal::WrongScope { .. }));
    assert!(refusal.reason().contains("/other/runtime"));
}

#[test]
fn a_state_for_a_different_target_is_refused() {
    let fd = handover::encode_state(&state()).unwrap();
    let decoded = handover::decode_state(fd.as_fd()).unwrap();
    let refusal = handover::validate_state(&decoded, &scope("/project/runtime"), &target("other"))
        .unwrap_err();
    assert!(matches!(refusal, HandoverRefusal::WrongTarget { .. }));
}

/// A lease the old guardian released must not come back: the client is supposed to be
/// gone, and handing it a live lease would resurrect it.
#[test]
fn a_released_lease_is_not_resurrected() {
    let mut carried = state();
    carried.released.push(ReleasedIdentity {
        client_run_id: "tui".into(),
        epoch: 2,
    });
    let fd = handover::encode_state(&carried).unwrap();
    let decoded = handover::decode_state(fd.as_fd()).unwrap();
    let refusal = handover::validate_state(&decoded, &scope("/project/runtime"), &target("next"))
        .unwrap_err();
    assert_eq!(refusal, HandoverRefusal::StaleRelease("tui".to_string()));
}

/// The carried credential is present in the state for the new program to authenticate
/// the client, so the *token* must not be carried alongside it -- only its identity.
#[test]
fn a_consumed_token_is_carried_as_an_identity_only() {
    let decoded =
        handover::decode_state(handover::encode_state(&state()).unwrap().as_fd()).unwrap();
    assert_eq!(decoded.consumed_tokens, vec!["token-identity".to_string()]);
    // A 64-character hex token would be a real confirmation token; an identity is a
    // short label. This pins the intent rather than the exact string.
    for identity in &decoded.consumed_tokens {
        assert!(
            identity.len() < 64,
            "a carried value must not look like a live token: {identity}"
        );
    }
}

/// An oversized state is refused at encode time, which is what lets the caller refuse
/// the handover *before* the old runtime stops.
#[test]
fn an_oversized_state_is_refused_before_anything_stops() {
    let mut huge = state();
    huge.consumed_tokens = (0..MAX_HANDOVER_BYTES / 8)
        .map(|index| format!("{index:08}"))
        .collect();
    let error = handover::encode_state(&huge).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("handover limit"));
}

/// The remaining budget is what is left, not a fresh one.
#[test]
fn the_handover_cost_is_subtracted_from_the_lease() {
    let captured = handover::cross_process_monotonic_ns();
    let now = captured + 4_000_000_000;
    assert_eq!(
        handover::remaining_after_capture(captured, now, 20_000),
        16_000
    );
}

/// The carried operation is the one a client can still poll, so a UI does not lose
/// the switch when the guardian restarts under it.
#[test]
fn the_carried_operation_survives_the_handover() {
    let decoded =
        handover::decode_state(handover::encode_state(&state()).unwrap().as_fd()).unwrap();
    assert_eq!(decoded.operation.operation_id, "op-1");
    assert_eq!(decoded.operation.phase, SwitchPhase::Starting);
}

/// Every descriptor role the receiver must recognise is carried by role, not by
/// number: a number means nothing on the other side of `exec`.
#[test]
fn descriptors_are_carried_by_role_not_by_number() {
    let decoded =
        handover::decode_state(handover::encode_state(&state()).unwrap().as_fd()).unwrap();
    let roles: Vec<DescriptorRole> = decoded.preserved.iter().map(|held| held.role).collect();
    assert_eq!(
        roles,
        vec![
            DescriptorRole::ControlListener,
            DescriptorRole::RegisteredConnection
        ]
    );
    // The registered connection names its client and epoch, which is how the receiver
    // validates that the connection still belongs to a live lease.
    let connection = &decoded.preserved[1];
    assert_eq!(connection.client_run_id.as_deref(), Some("tui"));
    assert_eq!(connection.epoch, Some(1));
}

/// An unknown future version is refused rather than read as the current one.
#[test]
fn a_future_version_is_refused() {
    let mut future = state();
    future.version = HANDOVER_VERSION + 1;
    let fd = handover::encode_state(&future).unwrap();
    assert_eq!(
        handover::decode_state(fd.as_fd()).unwrap_err(),
        HandoverRefusal::UnknownVersion(HANDOVER_VERSION + 1)
    );
}

/// The state descriptor must be private: two encodes must not share a buffer that one
/// reader could see the other's writes in.
#[test]
fn two_encodes_produce_independent_descriptors() {
    let first = handover::encode_state(&state()).unwrap();
    let mut second_state = state();
    second_state.guardian_revision = 99;
    let second = handover::encode_state(&second_state).unwrap();
    assert_eq!(
        handover::decode_state(first.as_fd())
            .unwrap()
            .guardian_revision,
        9
    );
    assert_eq!(
        handover::decode_state(second.as_fd())
            .unwrap()
            .guardian_revision,
        99
    );
}
