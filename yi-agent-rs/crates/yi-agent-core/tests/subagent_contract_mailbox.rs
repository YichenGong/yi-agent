use std::collections::BTreeSet;

use yi_agent_core::subagent::contract::{
    AuthorityDerivationError, DelegatedAuthority, EffectiveBudget, PathScope,
};
use yi_agent_core::subagent::mailbox::{
    Mailbox, MailboxMessageDraft, MessageKind, MessagePriority,
};
use yi_agent_core::subagent::task::{AttemptId, RootSessionId, TaskId};

fn authority(tools: &[&str], paths: &[&str]) -> DelegatedAuthority {
    DelegatedAuthority::new(
        TaskId::new(),
        TaskId::new(),
        RootSessionId::new(),
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
    let child = authority(&["read", "bash"], &["crates/**"]);

    assert!(matches!(
        parent.derive_child(&child),
        Err(AuthorityDerivationError::ToolNotDelegable { .. })
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
