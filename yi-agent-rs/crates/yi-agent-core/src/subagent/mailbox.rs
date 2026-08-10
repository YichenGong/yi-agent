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
    ReviewRejected(MessageId),
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
            | Self::Rework(_)
            | Self::ReviewRejected(_) => MessagePriority::High,
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
    pub sender: Option<TaskId>,
    pub recipient: TaskId,
    pub kind: MessageKind,
    pub priority: MessagePriority,
    pub correlation_id: Option<AttemptId>,
    pub created_at: DateTime<Utc>,
    pub coalesced_count: u32,
    /// In-memory acknowledgement that this daemon process handed the item to
    /// a worker inbox. Durable consumption is tracked separately by runtime.
    pub delivered_to_worker: bool,
    /// The worker has committed to injecting this external override into its
    /// next prompt. The runtime persists the corresponding acknowledgement.
    pub consumed_by_worker: bool,
}

#[derive(Debug, Clone)]
pub struct MailboxMessageDraft {
    id: Option<MessageId>,
    sender: Option<TaskId>,
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
            id: None,
            sender: Some(sender),
            recipient,
            kind,
            correlation_id,
        }
    }

    /// Reuses a durable mailbox identity allocated by the runtime repository.
    pub fn new_with_id(
        id: MessageId,
        sender: TaskId,
        recipient: TaskId,
        kind: MessageKind,
        correlation_id: Option<AttemptId>,
    ) -> Self {
        Self {
            id: Some(id),
            sender: Some(sender),
            recipient,
            kind,
            correlation_id,
        }
    }

    /// User interventions are external inputs, not forged task messages.
    pub fn user_override(recipient: TaskId, message: impl Into<String>) -> Self {
        Self {
            id: None,
            sender: None,
            recipient,
            kind: MessageKind::UserInstruction(UserInstruction(message.into())),
            correlation_id: None,
        }
    }

    /// Uses the ID allocated by the durable mailbox row.
    pub fn user_override_with_id(
        id: MessageId,
        recipient: TaskId,
        message: impl Into<String>,
    ) -> Self {
        Self {
            id: Some(id),
            sender: None,
            recipient,
            kind: MessageKind::UserInstruction(UserInstruction(message.into())),
            correlation_id: None,
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

    pub fn recipient(&self) -> &TaskId {
        &self.recipient
    }

    pub fn kind(&self) -> &MessageKind {
        &self.kind
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryReceipt {
    pub message_id: MessageId,
    pub wakes_recipient: bool,
    pub coalesced: bool,
}

#[derive(Debug, Clone, Default)]
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
            id: draft.id.unwrap_or_default(),
            sender: draft.sender,
            recipient: draft.recipient,
            kind: draft.kind,
            priority,
            correlation_id: draft.correlation_id,
            created_at: Utc::now(),
            coalesced_count: 1,
            delivered_to_worker: false,
            consumed_by_worker: false,
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

    pub fn is_rework(&self, id: &MessageId) -> bool {
        self.messages
            .iter()
            .any(|message| &message.id == id && matches!(message.kind, MessageKind::Rework(_)))
    }

    pub fn user_instruction(&self, id: &MessageId) -> Option<String> {
        self.messages
            .iter()
            .find(|message| &message.id == id)
            .and_then(|message| match &message.kind {
                MessageKind::UserInstruction(UserInstruction(body)) => Some(body.clone()),
                _ => None,
            })
    }

    /// External user overrides are durable pending input. A worker that starts
    /// after the user action must receive them before it can make progress.
    pub fn pending_user_overrides(&self) -> Vec<(MessageId, String)> {
        self.messages
            .iter()
            .filter_map(|message| match (&message.sender, &message.kind) {
                (None, MessageKind::UserInstruction(UserInstruction(body)))
                    if !message.consumed_by_worker =>
                {
                    Some((message.id.clone(), body.clone()))
                }
                _ => None,
            })
            .collect()
    }

    /// Controller inputs include external overrides and direct-parent rework
    /// instructions, both of which must reach a newly started worker prompt.
    pub fn pending_worker_inputs(&self) -> Vec<(MessageId, String)> {
        self.messages
            .iter()
            .filter_map(|message| match (&message.sender, &message.kind) {
                (None, MessageKind::UserInstruction(UserInstruction(body)))
                    if !message.consumed_by_worker =>
                {
                    Some((message.id.clone(), body.clone()))
                }
                (Some(_), MessageKind::Rework(ReworkInstruction(body)))
                    if !message.delivered_to_worker =>
                {
                    Some((message.id.clone(), body.clone()))
                }
                _ => None,
            })
            .collect()
    }

    pub fn mark_delivered_to_worker(&mut self, id: &MessageId) {
        if let Some(message) = self.messages.iter_mut().find(|message| &message.id == id) {
            message.delivered_to_worker = true;
        }
    }

    /// Only external user overrides participate in the durable consumption
    /// protocol. Agent-to-agent mail must not be mistaken for user input.
    pub fn mark_user_override_consumed(&mut self, id: &MessageId) -> bool {
        let Some(message) = self.messages.iter_mut().find(|message| &message.id == id) else {
            return false;
        };
        if message.sender.is_none()
            && matches!(message.kind, MessageKind::UserInstruction(_))
            && !message.consumed_by_worker
        {
            message.consumed_by_worker = true;
            true
        } else {
            false
        }
    }
}
