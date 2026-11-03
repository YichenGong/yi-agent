//! Round-trip tests for the executor wire contract.
//!
//! Two things are checked that a generic serde derive would hide: that no field of
//! `WorkerStart` is silently dropped on the way to the executor, and that a secret
//! never becomes printable by being wrapped for transport.

use super::*;
use yi_agent_core::message::Message;

fn workspace() -> WorkerWorkspace {
    WorkerWorkspace {
        lease_id: "11111111-1111-1111-1111-111111111111".parse().unwrap(),
        repository_root: PathBuf::from("/repo"),
        path: PathBuf::from("/repo/.worktrees/task"),
        branch: "feat/task".into(),
        parent_branch: "main".into(),
        base_commit: "0123456789abcdef0123456789abcdef01234567".into(),
    }
}

fn coding_start() -> WorkerStart {
    WorkerStart::new(TaskId::new(), AttemptId::new(), RootSessionId::new())
        .with_objective("write the thing")
        .with_workspace(workspace())
        .with_workspace_mode(ChildWriteMode::Coding)
        .with_inherited_sandbox(InheritedSandbox::WorkspaceWrite)
        .with_model("child-model")
        .with_message_capability("cap-secret")
        .with_initial_user_messages(vec![WorkerMessage {
            id: MessageId::new(),
            body: "preloaded".into(),
        }])
        .with_extra_roots(vec![PathBuf::from("/extra")])
        .with_fork_messages(vec![Message::user("parent said")])
}

/// Every documented field survives the trip, including the ones a `Default` would
/// quietly supply.
#[test]
fn a_coding_start_round_trips_every_field() {
    let start = coding_start();
    let wire = WorkerStartWire::from_start(
        &start,
        RootExecutionConfig {
            model: "root-model".into(),
            sandbox: Some(InheritedSandbox::DangerFullAccess),
            permission_mode: Some("accept-edits".into()),
            provider_profile: Some("profile-a".into()),
        },
    );
    let encoded = encode_payload(&wire).unwrap();
    let decoded: WorkerStartWire = serde_json::from_slice(&encoded).unwrap();
    let restored = decoded
        .to_start(tokio_util::sync::CancellationToken::new())
        .unwrap();

    assert_eq!(restored.task_id, start.task_id);
    assert_eq!(restored.attempt_id, start.attempt_id);
    assert_eq!(restored.root_session_id, start.root_session_id);
    assert_eq!(restored.workspace, start.workspace);
    assert_eq!(restored.workspace_mode, ChildWriteMode::Coding);
    assert_eq!(
        restored.inherited_sandbox,
        Some(InheritedSandbox::WorkspaceWrite)
    );
    assert_eq!(restored.objective, start.objective);
    assert_eq!(restored.initial_user_messages, start.initial_user_messages);
    assert_eq!(restored.fork_messages, start.fork_messages);
    assert_eq!(restored.extra_roots, start.extra_roots);
    // The child's own override wins over the root's effective model.
    assert_eq!(restored.model, "child-model");
    assert_eq!(restored.message_capability, "cap-secret");
    assert!(!restored.cancellation.is_cancelled());
}

/// A read-only start keeps the in-place shape and inherits the *root's* model, not
/// some earlier task's.
#[test]
fn a_read_only_start_inherits_the_roots_current_model() {
    let start = WorkerStart::new(TaskId::new(), AttemptId::new(), RootSessionId::new())
        .with_objective("look around")
        .with_workspace_mode(ChildWriteMode::ReadOnly);
    let wire = WorkerStartWire::from_start(
        &start,
        RootExecutionConfig {
            model: "root-model-b".into(),
            ..Default::default()
        },
    );
    let restored = wire
        .to_start(tokio_util::sync::CancellationToken::new())
        .unwrap();
    assert_eq!(restored.workspace_mode, ChildWriteMode::ReadOnly);
    assert!(restored.workspace.is_none());
    assert_eq!(restored.model, "root-model-b");
    assert!(restored.fork_messages.is_none(), "fork stays opt-in");
}

/// A secret must not become printable by being put on the wire.
#[test]
fn the_capability_is_redacted_in_debug_output() {
    let wire = WorkerStartWire::from_start(&coding_start(), RootExecutionConfig::default());
    let printed = format!("{wire:?}");
    assert!(!printed.contains("cap-secret"));
    assert!(printed.contains("redacted"));
}

/// Every event variant survives, including the ones a partial mapping would drop.
#[test]
fn every_worker_event_round_trips() {
    let events = vec![
        WorkerEvent::MessageConsumed {
            message_id: MessageId::new(),
        },
        WorkerEvent::Delivered(DeliveryReport::coding(
            "abc",
            "main",
            workspace().lease_id,
            "ok",
        )),
        WorkerEvent::Completed {
            report: "done".into(),
        },
        WorkerEvent::BudgetExhausted {
            report: "truncated".into(),
        },
        WorkerEvent::CompletedWithoutDelivery,
        WorkerEvent::Paused,
        WorkerEvent::Cancelled,
        WorkerEvent::Failed("boom".into()),
        WorkerEvent::RecoveryConflict("conflict".into()),
    ];
    for event in events {
        let wire = WorkerEventWire::from(event.clone());
        let encoded = encode_payload(&wire).unwrap();
        let decoded: WorkerEventWire = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.to_event().unwrap(), event);
    }
}

/// The property the whole design rests on.
#[test]
fn completed_event_does_not_imply_stopped_event() {
    let completed = ExecutorEvent::Worker {
        task: "t1".into(),
        attempt: "a1".into(),
        payload: WorkerEventWire::Completed {
            report: "done".into(),
        },
    };
    assert!(!matches!(completed, ExecutorEvent::Stopped { .. }));
}

#[test]
fn every_watchdog_event_round_trips() {
    let events = [
        WorkerWatchdogEvent::ProviderUsage {
            input_tokens: 10,
            output_tokens: 4,
            cache_creation_tokens: 2,
            cache_read_tokens: 8,
        },
        WorkerWatchdogEvent::ProviderRetry,
        WorkerWatchdogEvent::ToolRetry,
        WorkerWatchdogEvent::MeaningfulProgress,
    ];
    for event in events {
        let wire = WorkerWatchdogEventWire::from(event.clone());
        let encoded = encode_payload(&wire).unwrap();
        let decoded: WorkerWatchdogEventWire = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.to_event(), event);
    }
}

#[test]
fn trace_facts_and_gate_messages_round_trip() {
    let event = ExecutorEvent::Trace {
        task: "t".into(),
        attempt: "a".into(),
        payload: TraceFact::ToolCall {
            name: "bash".into(),
            summary: "ls".into(),
        },
    };
    let encoded = encode_payload(&event).unwrap();
    let decoded: ExecutorEvent = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(decoded, event);

    let acquire = GateMessage::AcquireTurn {
        request_id: "r1".into(),
        task: "t".into(),
        attempt: "a".into(),
        profile: "p".into(),
    };
    let encoded = encode_payload(&acquire).unwrap();
    assert_eq!(
        serde_json::from_slice::<GateMessage>(&encoded).unwrap(),
        acquire
    );
    assert_eq!(acquire.request_id(), Some("r1"));
}

/// A fork payload larger than a control frame but under 1 MiB is accepted.
#[test]
fn a_large_fork_payload_fits_the_executor_frame() {
    let mut start = coding_start();
    start.fork_messages = Some(vec![Message::user("x".repeat(256 * 1024))]);
    let wire = WorkerStartWire::from_start(&start, RootExecutionConfig::default());
    assert!(wire.ensure_frameable().is_ok());
}

/// Over the limit is an explicit error *before* the start is attempted.
#[test]
fn an_oversized_start_is_refused_before_it_is_sent() {
    let mut start = coding_start();
    start.fork_messages = Some(vec![Message::user("x".repeat(MAX_EXECUTOR_FRAME))]);
    let wire = WorkerStartWire::from_start(&start, RootExecutionConfig::default());
    let error = wire.ensure_frameable().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
}

/// A frame header above the limit is rejected without allocating the body.
#[test]
fn an_oversized_frame_header_is_rejected() {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&((MAX_EXECUTOR_FRAME as u32) + 1).to_be_bytes());
    let mut reader = std::io::Cursor::new(bytes);
    let error = read_frame::<_, ExecutorEvent>(&mut reader).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
}

/// A bounded inbox refuses rather than growing without limit.
#[test]
fn a_full_inbox_refuses_rather_than_dropping_silently() {
    let inbox = MessageInbox::new(2);
    assert!(inbox.push(WorkerMessage {
        id: MessageId::new(),
        body: "1".into()
    }));
    assert!(inbox.push(WorkerMessage {
        id: MessageId::new(),
        body: "2".into()
    }));
    assert!(!inbox.push(WorkerMessage {
        id: MessageId::new(),
        body: "3".into()
    }));
    assert_eq!(inbox.len(), 2);
    assert_eq!(inbox.drain().len(), 2);
    assert!(inbox.is_empty());
}

/// A Start's identity is what makes a replay detectable.
#[test]
fn a_start_carries_a_replay_identity() {
    let request = ExecutorRequest::Start {
        command_id: "c1".into(),
        generation: "g1".into(),
        owner: "app".into(),
        payload: Box::new(WorkerStartWire::from_start(
            &coding_start(),
            RootExecutionConfig::default(),
        )),
    };
    assert!(request.command_identity().is_some());
    assert!(
        ExecutorRequest::Cancel {
            task: "t".into(),
            attempt: "a".into()
        }
        .command_identity()
        .is_none()
    );
}
