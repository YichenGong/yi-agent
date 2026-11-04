//! The executor's wire contract: what crosses the process boundary, and what must
//! never cross it.
//!
//! The executor is a *separate process* per client. Everything the daemon knows
//! about a task therefore has to be serialised, and the automatic `Serialize` of
//! the core types is not enough: `WorkerStart` carries a cancellation token, which
//! is process-local, and `WorkerEvent` has no serde at all. This module supplies
//! explicit wire mirrors so a protocol change is a compile error rather than a
//! silently dropped field.
//!
//! Two deliberate properties:
//!
//! - `message_capability` is a secret. Its wire type redacts it in `Debug`, so a
//!   stray log line cannot leak it.
//! - Frames are capped at 1 MiB, and the cap is enforced *before* a start is
//!   attempted. A payload that cannot be framed is refused with an explicit error
//!   instead of a partial transfer that the other side has to guess about.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use yi_agent_core::message::Message;
use yi_agent_core::subagent::task::{
    AttemptId, ChildWriteMode, DeliveryReport, InheritedSandbox, MessageId, RootSessionId, TaskId,
    WorkspaceLeaseId,
};
use yi_agent_core::subagent::trace::TraceFact;
use yi_agent_core::subagent::worker::{
    WorkerEvent, WorkerMessage, WorkerStart, WorkerWatchdogEvent, WorkerWorkspace,
};

/// The largest frame the executor channel accepts, in bytes.
///
/// Deliberately larger than the 16 KiB control channel: a fork payload carries a
/// whole conversation, which cannot fit in a control frame. It is still capped so
/// a corrupt length header cannot make one side allocate without bound.
pub const MAX_EXECUTOR_FRAME: usize = 1024 * 1024;

/// A workspace assignment, mirrored for the wire.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerWorkspaceWire {
    pub lease_id: String,
    pub repository_root: PathBuf,
    pub path: PathBuf,
    pub branch: String,
    pub parent_branch: String,
    pub base_commit: String,
}

impl From<&WorkerWorkspace> for WorkerWorkspaceWire {
    fn from(workspace: &WorkerWorkspace) -> Self {
        Self {
            lease_id: workspace.lease_id.to_string(),
            repository_root: workspace.repository_root.clone(),
            path: workspace.path.clone(),
            branch: workspace.branch.clone(),
            parent_branch: workspace.parent_branch.clone(),
            base_commit: workspace.base_commit.clone(),
        }
    }
}

impl WorkerWorkspaceWire {
    pub fn to_workspace(&self) -> Result<WorkerWorkspace, String> {
        Ok(WorkerWorkspace {
            lease_id: self
                .lease_id
                .parse()
                .map_err(|error| format!("invalid workspace lease id: {error}"))?,
            repository_root: self.repository_root.clone(),
            path: self.path.clone(),
            branch: self.branch.clone(),
            parent_branch: self.parent_branch.clone(),
            base_commit: self.base_commit.clone(),
        })
    }
}

/// One queued user message, mirrored for the wire.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerMessageWire {
    pub id: String,
    pub body: String,
}

impl From<&WorkerMessage> for WorkerMessageWire {
    fn from(message: &WorkerMessage) -> Self {
        Self {
            id: message.id.to_string(),
            body: message.body.clone(),
        }
    }
}

impl WorkerMessageWire {
    pub fn to_message(&self) -> Result<WorkerMessage, String> {
        Ok(WorkerMessage {
            id: self
                .id
                .parse()
                .map_err(|error| format!("bad message id: {error}"))?,
            body: self.body.clone(),
        })
    }
}

/// The root's *effective* configuration, captured per Start.
///
/// Passed explicitly rather than inherited from "the first root this executor
/// saw": one executor serves several tasks for the same client, and a later task
/// with a different model or permission override must not silently run with the
/// earlier task's settings.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootExecutionConfig {
    pub model: String,
    /// The root's sandbox, when the daemon pinned one.
    pub sandbox: Option<InheritedSandbox>,
    /// Permission overrides such as `accept-edits`, opaque to the executor.
    pub permission_mode: Option<String>,
    /// Which provider profile the root selected.
    pub provider_profile: Option<String>,
}

/// A `WorkerStart` in wire form.
///
/// `Eq` is unavailable because `Message` carries a float-free but
/// non-`Eq` payload (`ImageDetail`); `PartialEq` is enough for round-trip tests.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerStartWire {
    pub task_id: String,
    pub attempt_id: String,
    pub root_session_id: String,
    pub workspace_lease_id: Option<String>,
    pub workspace: Option<WorkerWorkspaceWire>,
    pub workspace_mode: ChildWriteMode,
    pub inherited_sandbox: Option<InheritedSandbox>,
    /// Secret. Redacted by this type's `Debug`.
    pub message_capability: String,
    pub initial_user_messages: Vec<WorkerMessageWire>,
    pub objective: String,
    pub fork_messages: Option<Vec<Message>>,
    pub model: String,
    pub extra_roots: Vec<PathBuf>,
    pub root_execution: RootExecutionConfig,
}

impl std::fmt::Debug for WorkerStartWire {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerStartWire")
            .field("task_id", &self.task_id)
            .field("attempt_id", &self.attempt_id)
            .field("workspace_mode", &self.workspace_mode)
            .field("objective", &self.objective)
            .field("model", &self.model)
            // Never print the capability: it authorises mutations.
            .field(
                "message_capability",
                &if self.message_capability.is_empty() {
                    "<none>"
                } else {
                    "<redacted>"
                },
            )
            .finish_non_exhaustive()
    }
}

impl WorkerStartWire {
    /// Converts a live `WorkerStart` for the wire.
    ///
    /// The cancellation token stays behind on purpose: cancellation is carried by
    /// a [`ExecutorRequest::Cancel`] so the executor's token is the only one that
    /// can stop its own thread.
    pub fn from_start(start: &WorkerStart, root_execution: RootExecutionConfig) -> Self {
        Self {
            task_id: start.task_id.to_string(),
            attempt_id: start.attempt_id.to_string(),
            root_session_id: start.root_session_id.to_string(),
            workspace_lease_id: start.workspace_lease_id.as_ref().map(ToString::to_string),
            workspace: start.workspace.as_ref().map(WorkerWorkspaceWire::from),
            workspace_mode: start.workspace_mode,
            inherited_sandbox: start.inherited_sandbox,
            message_capability: start.message_capability.clone(),
            initial_user_messages: start
                .initial_user_messages
                .iter()
                .map(WorkerMessageWire::from)
                .collect(),
            objective: start.objective.clone(),
            fork_messages: start.fork_messages.clone(),
            model: start.model.clone(),
            extra_roots: start.extra_roots.clone(),
            root_execution,
        }
    }

    /// Rebuilds a `WorkerStart` on the executor side, with a fresh token.
    pub fn to_start(
        &self,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<WorkerStart, String> {
        let task_id: TaskId = self
            .task_id
            .parse()
            .map_err(|error| format!("bad task id: {error}"))?;
        let attempt_id: AttemptId = self
            .attempt_id
            .parse()
            .map_err(|error| format!("bad attempt id: {error}"))?;
        let root_session_id: RootSessionId = self
            .root_session_id
            .parse()
            .map_err(|error| format!("bad root session id: {error}"))?;
        let workspace = match &self.workspace {
            Some(workspace) => Some(workspace.to_workspace()?),
            None => None,
        };
        let workspace_lease_id: Option<WorkspaceLeaseId> = match &self.workspace_lease_id {
            Some(lease) => Some(
                lease
                    .parse()
                    .map_err(|error| format!("bad lease id: {error}"))?,
            ),
            None => None,
        };
        let mut start = WorkerStart::new(task_id, attempt_id, root_session_id);
        start.workspace_lease_id = workspace_lease_id;
        start.workspace = workspace;
        start.workspace_mode = self.workspace_mode;
        start.inherited_sandbox = self.inherited_sandbox;
        start.cancellation = cancellation;
        start.message_capability = self.message_capability.clone();
        start.initial_user_messages = self
            .initial_user_messages
            .iter()
            .map(WorkerMessageWire::to_message)
            .collect::<Result<Vec<_>, _>>()?;
        start.objective = self.objective.clone();
        start.fork_messages = self.fork_messages.clone();
        // The daemon's per-child override wins; the root's effective model is the
        // fallback, so a task that inherits takes *this root's* current model.
        start.model = if self.model.is_empty() {
            self.root_execution.model.clone()
        } else {
            self.model.clone()
        };
        start.extra_roots = self.extra_roots.clone();
        Ok(start)
    }

    /// The largest start this wire form may carry, checked before sending.
    pub fn ensure_frameable(&self) -> io::Result<()> {
        encode_payload(self)
            .map(|_| ())
            .map_err(|error| io::Error::new(error.kind(), error.to_string()))
    }
}

/// One worker event, mirrored variant by variant.
///
/// Deliberately *not* "any JSON": a new core variant must be added here by hand,
/// which is what keeps the executor from silently failing to report a state the
/// daemon needs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkerEventWire {
    MessageConsumed {
        message_id: String,
    },
    /// A coding delivery. Carries the whole report: the id is what a later
    /// acknowledgement refers to, and the file/limitation lists are what the
    /// reviewer reads. Dropping either would make the wire lossy.
    Delivered {
        id: String,
        commit: String,
        base_ref: String,
        workspace: String,
        evidence: String,
        #[serde(default)]
        changed_files: Vec<String>,
        #[serde(default)]
        known_limitations: Vec<String>,
    },
    Completed {
        report: String,
    },
    BudgetExhausted {
        report: String,
    },
    CompletedWithoutDelivery,
    Paused,
    Cancelled,
    Failed {
        message: String,
    },
    RecoveryConflict {
        message: String,
    },
}

impl From<WorkerEvent> for WorkerEventWire {
    fn from(event: WorkerEvent) -> Self {
        match event {
            WorkerEvent::MessageConsumed { message_id } => Self::MessageConsumed {
                message_id: message_id.to_string(),
            },
            WorkerEvent::Delivered(delivery) => Self::Delivered {
                id: delivery.id.to_string(),
                commit: delivery.commit,
                base_ref: delivery.base_ref,
                workspace: delivery.workspace.to_string(),
                evidence: delivery.evidence,
                changed_files: delivery.changed_files,
                known_limitations: delivery.known_limitations,
            },
            WorkerEvent::Completed { report } => Self::Completed { report },
            WorkerEvent::BudgetExhausted { report } => Self::BudgetExhausted { report },
            WorkerEvent::CompletedWithoutDelivery => Self::CompletedWithoutDelivery,
            WorkerEvent::Paused => Self::Paused,
            WorkerEvent::Cancelled => Self::Cancelled,
            WorkerEvent::Failed(message) => Self::Failed { message },
            WorkerEvent::RecoveryConflict(message) => Self::RecoveryConflict { message },
        }
    }
}

impl WorkerEventWire {
    pub fn to_event(&self) -> Result<WorkerEvent, String> {
        Ok(match self {
            Self::MessageConsumed { message_id } => WorkerEvent::MessageConsumed {
                message_id: message_id
                    .parse::<MessageId>()
                    .map_err(|error| format!("bad message id: {error}"))?,
            },
            Self::Delivered {
                id,
                commit,
                base_ref,
                workspace,
                evidence,
                changed_files,
                known_limitations,
            } => {
                let mut report = DeliveryReport::coding(
                    commit.clone(),
                    base_ref.clone(),
                    workspace
                        .parse()
                        .map_err(|error| format!("bad workspace id: {error}"))?,
                    evidence.clone(),
                );
                // The id identifies *this* delivery for acknowledgement; minting a
                // fresh one here would make the daemon's record and the executor's
                // record disagree.
                report.id = id
                    .parse()
                    .map_err(|error| format!("bad delivery id: {error}"))?;
                report.changed_files = changed_files.clone();
                report.known_limitations = known_limitations.clone();
                WorkerEvent::Delivered(report)
            }
            Self::Completed { report } => WorkerEvent::Completed {
                report: report.clone(),
            },
            Self::BudgetExhausted { report } => WorkerEvent::BudgetExhausted {
                report: report.clone(),
            },
            Self::CompletedWithoutDelivery => WorkerEvent::CompletedWithoutDelivery,
            Self::Paused => WorkerEvent::Paused,
            Self::Cancelled => WorkerEvent::Cancelled,
            Self::Failed { message } => WorkerEvent::Failed(message.clone()),
            Self::RecoveryConflict { message } => WorkerEvent::RecoveryConflict(message.clone()),
        })
    }
}

/// One watchdog fact, mirrored variant by variant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkerWatchdogEventWire {
    ProviderUsage {
        input_tokens: u64,
        output_tokens: u64,
        cache_creation_tokens: u64,
        cache_read_tokens: u64,
    },
    ProviderRetry,
    ToolRetry,
    MeaningfulProgress,
}

impl From<WorkerWatchdogEvent> for WorkerWatchdogEventWire {
    fn from(event: WorkerWatchdogEvent) -> Self {
        match event {
            WorkerWatchdogEvent::ProviderUsage {
                input_tokens,
                output_tokens,
                cache_creation_tokens,
                cache_read_tokens,
            } => Self::ProviderUsage {
                input_tokens,
                output_tokens,
                cache_creation_tokens,
                cache_read_tokens,
            },
            WorkerWatchdogEvent::ProviderRetry => Self::ProviderRetry,
            WorkerWatchdogEvent::ToolRetry => Self::ToolRetry,
            WorkerWatchdogEvent::MeaningfulProgress => Self::MeaningfulProgress,
        }
    }
}

impl WorkerWatchdogEventWire {
    pub fn to_event(&self) -> WorkerWatchdogEvent {
        match *self {
            Self::ProviderUsage {
                input_tokens,
                output_tokens,
                cache_creation_tokens,
                cache_read_tokens,
            } => WorkerWatchdogEvent::ProviderUsage {
                input_tokens,
                output_tokens,
                cache_creation_tokens,
                cache_read_tokens,
            },
            Self::ProviderRetry => WorkerWatchdogEvent::ProviderRetry,
            Self::ToolRetry => WorkerWatchdogEvent::ToolRetry,
            Self::MeaningfulProgress => WorkerWatchdogEvent::MeaningfulProgress,
        }
    }
}

/// A request the daemon sends to one client's executor.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "snake_case")]
pub enum ExecutorRequest {
    Start {
        command_id: String,
        generation: String,
        owner: String,
        /// Boxed: a `Start` carries a whole conversation, while every other
        /// request is a few small strings.
        payload: Box<WorkerStartWire>,
    },
    Cancel {
        task: String,
        attempt: String,
    },
    Pause {
        task: String,
        attempt: String,
    },
    Message {
        task: String,
        attempt: String,
        id: String,
        body: String,
    },
}

impl ExecutorRequest {
    /// The non-idempotent identity of a `Start`.
    ///
    /// A reconnect must not replay a Start whose effect already happened, so the
    /// executor remembers these and answers a repeat with the original outcome.
    pub fn command_identity(&self) -> Option<(&str, &str, &str, &str)> {
        match self {
            Self::Start {
                command_id,
                generation,
                owner,
                payload,
            } => Some((
                command_id.as_str(),
                generation.as_str(),
                owner.as_str(),
                payload.task_id.as_str(),
            )),
            _ => None,
        }
    }
}

/// An event the executor sends back.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ExecutorEvent {
    Worker {
        task: String,
        attempt: String,
        payload: WorkerEventWire,
    },
    Watchdog {
        task: String,
        attempt: String,
        payload: WorkerWatchdogEventWire,
    },
    Trace {
        task: String,
        attempt: String,
        payload: TraceFact,
    },
    /// The execution unit has really joined. Never implied by `Worker`.
    Stopped { task: String, attempt: String },
    /// A `Start` was refused before anything ran.
    Rejected { command_id: String, reason: String },
}

/// The provider-turn gate sub-protocol.
///
/// The *daemon* holds the real permit: the executor only ever holds a proxy, so a
/// wedged or crashed executor cannot grant itself extra turns, and the global
/// concurrency limit is enforced on one machine-wide object.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "gate", rename_all = "snake_case")]
pub enum GateMessage {
    AcquireTurn {
        request_id: String,
        task: String,
        attempt: String,
        profile: String,
    },
    TurnGranted {
        request_id: String,
        permit_id: String,
    },
    ReleaseTurn {
        permit_id: String,
    },
    /// The daemon refused to grant a turn. Explicit rather than silence: an
    /// executor that only timed out could not tell a full gate from a dead daemon.
    Denied {
        request_id: String,
        reason: String,
    },
}

/// A frame the daemon sends to an executor.
///
/// Requests and gate replies share one socket but not one envelope: a
/// `TurnGranted` is not an `ExecutorRequest`, and before this envelope existed the
/// grant was written into the request stream, where the executor's
/// `ExecutorRequest` parser rejected it. Both inner enums are internally tagged
/// (`request` / `gate`), so `untagged` dispatch is unambiguous.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DaemonFrame {
    /// Boxed: a `Start` carries a whole conversation, and every gate round-trip
    /// would otherwise move that much memory through the frame enum.
    Request(Box<ExecutorRequest>),
    Gate(GateMessage),
}

/// A frame an executor sends to the daemon.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ExecutorFrame {
    Event(Box<ExecutorEvent>),
    Gate(GateMessage),
}

impl GateMessage {
    /// The `(task, attempt)` this message concerns, when it names one.
    pub fn waiter(&self) -> Option<(String, String)> {
        match self {
            GateMessage::AcquireTurn { task, attempt, .. } => Some((task.clone(), attempt.clone())),
            _ => None,
        }
    }

    /// Whether this message may travel daemon-to-executor.
    ///
    /// `AcquireTurn` is the executor's question; a daemon that sent one would be
    /// asking itself for a permit it already owns.
    pub fn is_daemon_bound(&self) -> bool {
        matches!(
            self,
            Self::TurnGranted { .. } | Self::Denied { .. } | Self::ReleaseTurn { .. }
        )
    }

    /// Whether this message may travel executor-to-daemon.
    pub fn is_executor_bound(&self) -> bool {
        matches!(self, Self::AcquireTurn { .. } | Self::ReleaseTurn { .. })
    }

    pub fn request_id(&self) -> Option<&str> {
        match self {
            Self::AcquireTurn { request_id, .. } => Some(request_id),
            Self::TurnGranted { request_id, .. } => Some(request_id),
            Self::ReleaseTurn { .. } | Self::Denied { .. } => None,
        }
    }
}

/// Writes one length-prefixed frame, refusing oversized payloads up front.
pub fn write_frame<W: Write, T: Serialize>(writer: &mut W, value: &T) -> io::Result<()> {
    let body = encode_payload(value)?;
    let length = (body.len() as u32).to_be_bytes();
    writer.write_all(&length)?;
    writer.write_all(&body)?;
    writer.flush()
}

/// Encodes a payload, applying the 1 MiB cap before any bytes are written.
pub fn encode_payload<T: Serialize>(value: &T) -> io::Result<Vec<u8>> {
    let body = serde_json::to_vec(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if body.len() > MAX_EXECUTOR_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "executor frame is {} bytes, over the {MAX_EXECUTOR_FRAME}-byte limit",
                body.len()
            ),
        ));
    }
    Ok(body)
}

/// Reads one length-prefixed frame, rejecting an over-limit header before
/// allocating the body.
pub fn read_frame<R: Read, T: for<'de> Deserialize<'de>>(reader: &mut R) -> io::Result<T> {
    let mut header = [0u8; 4];
    reader.read_exact(&mut header)?;
    let length = u32::from_be_bytes(header) as usize;
    if length > MAX_EXECUTOR_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("executor frame of {length} bytes exceeds the {MAX_EXECUTOR_FRAME}-byte limit"),
        ));
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// A bounded FIFO of messages for one running attempt.
///
/// Bounded on purpose: a client that floods its worker must not be able to grow
/// the executor's memory without limit while the worker is busy.
#[derive(Debug)]
pub struct MessageInbox {
    queue: Arc<Mutex<VecDeque<WorkerMessage>>>,
    capacity: usize,
}

impl MessageInbox {
    pub fn new(capacity: usize) -> Self {
        Self {
            queue: Arc::new(Mutex::new(VecDeque::new())),
            capacity,
        }
    }

    /// Queues one message. Returns `false` when the inbox is full, so the caller
    /// can report a refusal instead of dropping the message silently.
    pub fn push(&self, message: WorkerMessage) -> bool {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if queue.len() >= self.capacity {
            return false;
        }
        queue.push_back(message);
        true
    }

    pub fn drain(&self) -> Vec<WorkerMessage> {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        queue.drain(..).collect()
    }

    pub fn len(&self) -> usize {
        self.queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A bounded read/write deadline pair for an executor socket.
pub fn set_frame_timeouts(
    stream: &std::os::unix::net::UnixStream,
    budget: Duration,
) -> io::Result<()> {
    stream.set_read_timeout(Some(budget))?;
    stream.set_write_timeout(Some(budget))?;
    Ok(())
}

#[cfg(test)]
mod tests;
