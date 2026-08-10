use std::collections::BTreeSet;

use yi_agent_core::subagent::contract::{
    AuthorityDerivationError, DelegatedAuthority, EffectiveBudget, PathScope,
};
use yi_agent_core::subagent::mailbox::{
    Mailbox, MailboxMessageDraft, MessageKind, MessagePriority, ReworkInstruction,
};
use yi_agent_core::subagent::task::{AttemptId, MessageId, RootSessionId, TaskId};

fn authority(tools: &[&str], paths: &[&str]) -> DelegatedAuthority {
    authority_for_root(RootSessionId::new(), tools, paths)
}

fn authority_for_root(
    root_session_id: RootSessionId,
    tools: &[&str],
    paths: &[&str],
) -> DelegatedAuthority {
    DelegatedAuthority::new(
        TaskId::new(),
        TaskId::new(),
        root_session_id,
        tools
            .iter()
            .map(|tool| (*tool).to_string())
            .collect::<BTreeSet<_>>(),
        PathScope::new(paths.iter().map(|path| (*path).to_string()).collect()),
        EffectiveBudget {
            max_turns: Some(20),
            max_wall_time_secs: Some(600),
        },
        None,
    )
}

#[test]
fn child_authority_cannot_widen_parent_tools_or_paths() {
    let parent = authority(&["read", "write"], &["crates/core/**"]);
    let child = authority_for_root(
        parent.root_session_id.clone(),
        &["read", "bash"],
        &["crates/**"],
    );

    assert!(matches!(
        parent.derive_child(&child),
        Err(AuthorityDerivationError::ToolNotDelegable { .. })
    ));
}

#[test]
fn child_authority_cannot_cross_root_session_boundary() {
    let parent = authority(&["read"], &["crates/core/**"]);
    let child = authority(&["read"], &["crates/core/src/**"]);

    assert!(matches!(
        parent.derive_child(&child),
        Err(AuthorityDerivationError::RootSessionMismatch)
    ));
}

#[test]
fn repeated_progress_coalesces_without_waking_the_parent() {
    let parent = TaskId::new();
    let child = TaskId::new();
    let correlation_id = AttemptId::new();
    let mut mailbox = Mailbox::default();

    let first = mailbox.push(MailboxMessageDraft::progress(
        child.clone(),
        parent.clone(),
        correlation_id.clone(),
        "first update",
    ));
    let second = mailbox.push(MailboxMessageDraft::progress(
        child,
        parent,
        correlation_id,
        "latest update",
    ));

    assert!(!first.wakes_recipient);
    assert!(!second.wakes_recipient);
    assert_eq!(mailbox.messages().len(), 1);
    assert_eq!(mailbox.messages()[0].coalesced_count, 2);
    assert!(matches!(
        mailbox.messages()[0].kind,
        MessageKind::Progress(_)
    ));
    assert_eq!(mailbox.messages()[0].priority, MessagePriority::Normal);
}

#[test]
fn completion_message_wakes_the_parent_and_is_not_coalesced() {
    let parent = TaskId::new();
    let child = TaskId::new();
    let mut mailbox = Mailbox::default();

    let receipt = mailbox.push(MailboxMessageDraft::new(
        child,
        parent,
        MessageKind::Blocked(yi_agent_core::subagent::task::BlockReason(
            "needs input".into(),
        )),
        None,
    ));

    assert!(receipt.wakes_recipient);
    assert!(!receipt.coalesced);
    assert_eq!(mailbox.messages().len(), 1);
    assert_eq!(mailbox.messages()[0].priority, MessagePriority::High);
}

#[test]
fn delivered_rework_instruction_is_not_replayed_to_a_later_worker() {
    let parent = TaskId::new();
    let child = TaskId::new();
    let message_id = MessageId::new();
    let mut mailbox = Mailbox::default();
    mailbox.push(MailboxMessageDraft::new_with_id(
        message_id.clone(),
        parent,
        child,
        MessageKind::Rework(ReworkInstruction("fix the parser".into())),
        Some(AttemptId::new()),
    ));

    assert_eq!(mailbox.pending_worker_inputs().len(), 1);
    mailbox.mark_delivered_to_worker(&message_id);

    assert!(mailbox.pending_worker_inputs().is_empty());
}
