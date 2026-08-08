use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::task::{
    AttemptId, BlockReason, DeliveryReport, MessageId, PermissionRequestId, TaskFailure, TaskId,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum MessagePriority {
    Critical,
    High,
    Normal,
    Background,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressReport(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeChangeDraft(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReworkInstruction(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserInstruction(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageKind {
    Progress(ProgressReport),
    Completed(DeliveryReport),
    Blocked(BlockReason),
    Failed(TaskFailure),
    PermissionRequest(PermissionRequestId),
    ScopeChange(ScopeChangeDraft),
    Rework(ReworkInstruction),
    UserInstruction(UserInstruction),
}

impl MessageKind {
    fn priority(&self) -> MessagePriority {
        match self {
            Self::PermissionRequest(_) => MessagePriority::Critical,
            Self::Completed(_)
            | Self::Blocked(_)
            | Self::Failed(_)
            | Self::ScopeChange(_)
            | Self::Rework(_) => MessagePriority::High,
            Self::Progress(_) | Self::UserInstruction(_) => MessagePriority::Normal,
        }
    }

    fn wakes_recipient(&self) -> bool {
        !matches!(self, Self::Progress(_))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailboxMessage {
    pub id: MessageId,
    pub sender: TaskId,
    pub recipient: TaskId,
    pub kind: MessageKind,
    pub priority: MessagePriority,
    pub correlation_id: Option<AttemptId>,
    pub created_at: DateTime<Utc>,
    pub coalesced_count: u32,
}

#[derive(Debug, Clone)]
pub struct MailboxMessageDraft {
    sender: TaskId,
    recipient: TaskId,
    kind: MessageKind,
    correlation_id: Option<AttemptId>,
}

impl MailboxMessageDraft {
    pub fn new(
        sender: TaskId,
        recipient: TaskId,
        kind: MessageKind,
        correlation_id: Option<AttemptId>,
    ) -> Self {
        Self {
            sender,
            recipient,
            kind,
            correlation_id,
        }
    }

    pub fn progress(
        sender: TaskId,
        recipient: TaskId,
        correlation_id: AttemptId,
        text: impl Into<String>,
    ) -> Self {
        Self::new(
            sender,
            recipient,
            MessageKind::Progress(ProgressReport(text.into())),
            Some(correlation_id),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryReceipt {
    pub message_id: MessageId,
    pub wakes_recipient: bool,
    pub coalesced: bool,
}

#[derive(Debug, Default)]
pub struct Mailbox {
    messages: Vec<MailboxMessage>,
}

impl Mailbox {
    pub fn push(&mut self, draft: MailboxMessageDraft) -> DeliveryReceipt {
        if let (MessageKind::Progress(_), Some(correlation_id)) =
            (&draft.kind, &draft.correlation_id)
        {
            if let Some(existing) = self.messages.iter_mut().find(|message| {
                matches!(message.kind, MessageKind::Progress(_))
                    && message.sender == draft.sender
                    && message.recipient == draft.recipient
                    && message.correlation_id.as_ref() == Some(correlation_id)
            }) {
                existing.kind = draft.kind;
                existing.coalesced_count += 1;
                return DeliveryReceipt {
                    message_id: existing.id.clone(),
                    wakes_recipient: false,
                    coalesced: true,
                };
            }
        }
        let priority = draft.kind.priority();
        let wakes_recipient = draft.kind.wakes_recipient();
        let message = MailboxMessage {
            id: MessageId::new(),
            sender: draft.sender,
            recipient: draft.recipient,
            kind: draft.kind,
            priority,
            correlation_id: draft.correlation_id,
            created_at: Utc::now(),
            coalesced_count: 1,
        };
        let receipt = DeliveryReceipt {
            message_id: message.id.clone(),
            wakes_recipient,
            coalesced: false,
        };
        self.messages.push(message);
        receipt
    }

    pub fn messages(&self) -> &[MailboxMessage] {
        &self.messages
    }
}
