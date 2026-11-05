use super::*;

/// Bumped whenever the request/response wire shape changes. `PluginQuery` is an
/// additive variant, but `IpcRequest` is `deny_unknown_fields`, so a v1 daemon
/// would reject a v2 request outright. Bumping makes the mismatch explicit and
/// reachable through `UnsupportedProtocol` rather than a bare parse failure.
pub const PROTOCOL_VERSION: u32 = 2;

pub(crate) const MAX_FRAME_BYTES: usize = 1024 * 1024;

pub(crate) const MAX_PENDING_EVENT_FRAMES: usize = 1024;

pub(crate) const MISSING_REQUEST_ID: &str = "";

pub(crate) static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestEnvelope {
    pub protocol_version: u32,
    pub request_id: String,
    #[serde(rename = "command")]
    pub command: IpcRequest,
}

/// One versioned response frame. Subscription event frames additionally carry
/// their durable event ID while retaining the subscription request correlation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseEnvelope {
    pub protocol_version: u32,
    pub request_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event: Option<IpcEventPayload>,
    pub result: IpcResponse,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
#[serde(deny_unknown_fields)]
pub enum IpcRequest {
    Status,
    Stop,
    CreateSession,
    AttachApplicationRoot {
        idempotency_key: String,
        workspace: PathBuf,
        /// The root session's linked roots (canonical absolute paths). An older
        /// client omits the field, which reads as "no linked roots".
        ///
        /// Additive and `#[serde(default)]`: a new daemon reads an old client's request
        /// as "no linked roots", unchanged. The reverse is not compatible: an old daemon
        /// rejects it as a bare parse error (`Validation`, no message), since
        /// `deny_unknown_fields` covers this internally-tagged variant (ipc.rs:178). A bump
        /// would not fix that (the command is parsed before `protocol_version`, 1492-1493)
        /// and would only force the unrelated plugin crate to bump; no bump, per `fork_token`.
        #[serde(default)]
        extra_roots: Vec<String>,
    },
    ActivateApplicationRoot {
        session_id: String,
        root_task_id: String,
        capability: String,
        objective: String,
    },
    DetachApplicationRoot {
        session_id: String,
        root_task_id: String,
        capability: String,
    },
    /// Binds the one driver allowed to report this root's activity.
    ///
    /// The authorization is the guardian-issued one, not a client-reported id: the
    /// daemon validates it against its own authority table, so a tool process cannot
    /// obtain a driver by naming an owner. `operation_id` is the identity of the bind:
    /// a retried request replays the issued epoch instead of minting a second one.
    BindApplicationRootDriver {
        authorization: yi_agent_lifecycle::types::RootAuthorization,
        operation_id: String,
    },
    /// Reports what the conversation is doing, under the driver's own fence.
    ///
    /// This is not a heartbeat: applying a report never extends the owner's lease and
    /// never marks progress. A report that is stale, from another epoch, or about
    /// another turn is refused with a stable code.
    SetApplicationRootActivity {
        driver_capability: yi_agent_lifecycle::foreground::SecretCapability,
        update: yi_agent_lifecycle::foreground::ActivityUpdate,
    },
    /// Reads the committed activity of one root. Never writes, never extends.
    GetApplicationRootActivity {
        authorization: yi_agent_lifecycle::types::RootAuthorization,
        root: yi_agent_lifecycle::foreground::RootRef,
    },
    /// Freezes and stops a root's executions, then offers a confirmation if proven.
    ///
    /// Separate from the commit so the host can show the impact first. Not a model
    /// tool: the confirmation has to come from a party the user actually sees.
    PrepareApplicationRootRecovery {
        authorization: yi_agent_lifecycle::types::RootAuthorization,
        request: yi_agent_lifecycle::foreground::RecoveryRequest,
    },
    /// Commits a prepared recovery against the confirmation the daemon issued.
    RecoverApplicationRoot {
        authorization: yi_agent_lifecycle::types::RootAuthorization,
        request: yi_agent_lifecycle::foreground::RecoveryRequest,
        confirmation_token: yi_agent_lifecycle::foreground::SecretCapability,
    },
    /// Reads one recovery operation's state, after re-authenticating the caller.
    GetApplicationRootRecovery {
        authorization: yi_agent_lifecycle::types::RootAuthorization,
        root: yi_agent_lifecycle::foreground::RootRef,
        operation_id: String,
    },
    CreateSchedule {
        cron: String,
        objective: String,
    },
    ListSchedules,
    DeleteSchedule {
        schedule_id: String,
    },
    SpawnChild {
        session_id: String,
        parent_task_id: String,
        objective: String,
        /// The turn that authorizes this write. Optional only so an older client
        /// still decodes; a container write without it is refused, never admitted.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_turn: Option<yi_agent_lifecycle::session_delegation::SessionTurn>,
        #[serde(default)]
        mode: Option<String>,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        workdir: Option<String>,
        #[serde(default)]
        sandbox: Option<String>,
        /// A completed fork upload to seed the child with. Additive and
        /// optional: an older client omits it, which reads as `None` and
        /// spawns exactly as before.
        #[serde(default)]
        fork_token: Option<String>,
    },
    SpawnApplicationChild {
        session_id: String,
        parent_task_id: String,
        capability: String,
        objective: String,
        /// The turn that authorizes this write. Optional only so an older client
        /// still decodes; a container write without it is refused, never admitted.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_turn: Option<yi_agent_lifecycle::session_delegation::SessionTurn>,
        #[serde(default)]
        mode: Option<String>,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        workdir: Option<String>,
        /// The conversation this child belongs to. Additive and optional: an
        /// older client that omits it deserializes to `None` and the daemon
        /// inherits the parent's marker, or leaves the child unbound.
        #[serde(default)]
        thread_id: Option<String>,
        #[serde(default)]
        sandbox: Option<String>,
        /// A completed fork upload to seed the child with. Additive and
        /// optional, like `thread_id`: omitting it spawns an unforked child.
        #[serde(default)]
        fork_token: Option<String>,
    },
    /// Opens a fork-payload upload for an authorized application root. The
    /// caller then pushes the payload as base64 chunks and names the token on a
    /// spawn request. Kept separate from the spawn so a payload larger than one
    /// frame still fits the 1 MiB frame cap: each chunk travels on its own.
    BeginForkUpload {
        session_id: String,
        caller_task_id: String,
        capability: String,
        total_bytes: u64,
        /// The turn that authorizes this write. Optional only so an older client
        /// still decodes; a container write without it is refused, never admitted.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_turn: Option<yi_agent_lifecycle::session_delegation::SessionTurn>,
    },
    /// Appends one base64 chunk to an open upload. `seq` is strict, so a gap,
    /// a replay, or a chunk past the declared length is rejected rather than
    /// silently corrupting the payload.
    AppendForkChunk {
        fork_token: String,
        seq: u64,
        data: String,
    },
    /// Drops an open upload. The token is invalid afterwards.
    AbortForkUpload {
        fork_token: String,
    },
    StartWorker {
        session_id: String,
        task_id: String,
    },
    /// Creates a root session that runs `objective` autonomously in `workdir`
    /// and starts its worker. Generic: the daemon does not interpret the
    /// objective, and `workdir` must already exist as a git worktree.
    CreateAutonomousSession {
        objective: String,
        workdir: String,
    },
    CancelTask {
        session_id: String,
        task_id: String,
        recursive: bool,
    },
    /// Cancels every live child a conversation owns.
    ///
    /// A conversation shares one attached root with its directory siblings, so
    /// routing a deletion through the root would cancel work belonging to the
    /// other conversations. The conversation marker is the only thing that
    /// separates their children, so this request takes the marker, not a task
    /// id, and the daemon resolves the set itself. Confirmation is the caller's
    /// own decision to delete: no preview token is required, unlike
    /// `CancelTask`/`ConfirmCancel`.
    CancelThreadTasks {
        session_id: String,
        thread_id: String,
        /// Whether the caller has already accepted that live children die with
        /// the conversation. `false` makes the daemon refuse without cancelling
        /// anything when children are still running, so the caller can ask the
        /// user first. The check and the cancellation run in one handler, so no
        /// child can appear between "may I?" and "go ahead".
        #[serde(default)]
        force: bool,
    },
    PreviewCancel {
        task_id: String,
        recursive: bool,
    },
    ConfirmCancel {
        task_id: String,
        recursive: bool,
        confirmation_token: String,
    },
    RetryTask {
        session_id: String,
        task_id: String,
    },
    PauseTask {
        session_id: String,
        task_id: String,
    },
    ResumeTask {
        session_id: String,
        task_id: String,
    },
    ResolvePermission {
        request_id: String,
        decision: IpcPermissionDecision,
    },
    SendMessage {
        session_id: String,
        sender_task_id: String,
        worker_capability: String,
        recipient_task_id: String,
        message: String,
    },
    SendApplicationMessage {
        session_id: String,
        sender_task_id: String,
        capability: String,
        recipient_task_id: String,
        message: String,
        /// The turn that authorizes this write. Optional only so an older client
        /// still decodes; a container write without it is refused, never admitted.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_turn: Option<yi_agent_lifecycle::session_delegation::SessionTurn>,
    },
    SendUserMessage {
        task_id: String,
        message: String,
    },
    WaitAgent {
        session_id: String,
        caller_task_id: String,
        capability: String,
        mode: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
    },
    InspectTask {
        task_id: String,
    },
    /// A parent reading one of its own descendants. Authorized by the
    /// application-root capability or the caller's own worker capability.
    InspectChild {
        session_id: String,
        caller_task_id: String,
        capability: String,
        task_id: String,
    },
    /// A parent cancelling one of its own descendants.
    CancelChild {
        session_id: String,
        caller_task_id: String,
        capability: String,
        task_id: String,
        #[serde(default)]
        recursive: bool,
        /// The turn that authorizes this write. Optional only so an older client
        /// still decodes; a container write without it is refused, never admitted.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_turn: Option<yi_agent_lifecycle::session_delegation::SessionTurn>,
    },
    ListTaskSummaries {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(default)]
        active_only: bool,
    },
    ReadTaskEvents {
        task_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after_event_id: Option<i64>,
    },
    ReadTaskMailbox {
        task_id: String,
    },
    ReadTaskDiff {
        task_id: String,
    },
    ReadTaskTrace {
        task_id: String,
    },
    /// Follows one or more tasks' traces live.
    ///
    /// `task_ids` is REQUIRED to hold at least one valid task id. The store
    /// reads a trace per task and has no "every task" cursor, so an empty or
    /// wholly invalid list is rejected with a validation error rather than
    /// silently answering like an idle stream.
    ///
    /// `kinds` filters on the RAW trace kind the writer stored (e.g.
    /// `assistant_text`, `tool_call`), not on a derived runtime event name, so a
    /// value that is not a trace kind matches nothing. Empty means every kind.
    SubscribeTrace {
        task_ids: Vec<String>,
        #[serde(default)]
        after_id: Option<i64>,
        #[serde(default)]
        kinds: Vec<String>,
    },
    SubscribeEvents {
        after_event_id: i64,
        #[serde(default)]
        filters: SubscriptionFilters,
    },
    /// Ask a supervised plugin a question, verbatim.
    ///
    /// The daemon forwards `method`/`params` and returns whatever comes back; it
    /// attaches no meaning to either. Which methods exist is a contract between
    /// the caller and the plugin, not something the daemon knows.
    PluginQuery {
        plugin: String,
        method: String,
        #[serde(default)]
        params: serde_json::Value,
    },
    /// 询问一个插件此刻是否处于维护屏障之下。
    ///
    /// # 为什么这不是"通知"而是"重新查一次"
    ///
    /// 请求里**没有**"现在该停吗"的字段。结论永远来自磁盘上的生命周期
    /// 日志，见 [`crate::ipc::MaintenanceBarrier`]。让调用方声明结论等于给它
    /// 一条绕过屏障的后门：一条伪造的"不用停"就能把后端拉起来。
    ///
    /// `protocol` 是发起方支持的维护协议版本。缺失（旧客户端）会被当作
    /// "不支持"，返回明确的兼容性错误而不是静默放行。
    PluginMaintenance {
        plugin: String,
        #[serde(default)]
        protocol: Option<u32>,
    },
    /// Attaches a session *container*: a root that owns delegation instead of
    /// running a worker. Separate from `AttachApplicationRoot` because the two
    /// answer different questions, and because an old client must keep getting
    /// the old answer.
    AttachSessionContainer {
        idempotency_key: String,
        workspace: PathBuf,
        #[serde(default)]
        extra_roots: Vec<String>,
        operation_id: String,
        /// The resume digest this container should accept from now on, written
        /// only when the container is created.
        #[serde(default)]
        creation_resume_digest: Option<String>,
        #[serde(default)]
        identity: Option<yi_agent_lifecycle::session_delegation::SessionIdentity>,
    },
    /// (Re)establishes the host binding and, only on success, returns the secret.
    PrepareSessionDelegation {
        request: yi_agent_lifecycle::session_delegation::PrepareSession,
    },
    BeginSessionTurn {
        binding: yi_agent_lifecycle::session_delegation::SessionBinding,
        turn_id: String,
    },
    EndSessionTurn {
        turn: yi_agent_lifecycle::session_delegation::SessionTurn,
    },
    /// Opens the host-private control stream for one bound session.
    ///
    /// Not a model tool. It must be opened with the *binding* capability, which
    /// never leaves the host; a turn secret is refused, so a tool process cannot
    /// register an execution or claim one stopped.
    OpenSessionHostControl {
        binding: yi_agent_lifecycle::session_delegation::SessionBinding,
    },
}

/// One frame on the host control stream, after `OpenSessionHostControl`.
///
/// The stream has its own request DTO on purpose: echoing it as an `IpcRequest`
/// would make the control operations reachable on any ordinary connection, which
/// is exactly the bypass the capability gate exists to prevent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionHostControlFrame {
    pub request_id: String,
    pub request: SessionHostControlRequest,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
#[serde(deny_unknown_fields)]
pub enum SessionHostControlRequest {
    /// Registers the current turn's execution before it can do any work.
    Register { turn_id: String },
    /// Reports a stop proof for an execution this stream registered.
    ///
    /// Boxed because a `StopEvidence` is far larger than the other variant and
    /// every control frame would otherwise be sized for it.
    Stopped {
        evidence: Box<yi_agent_lifecycle::foreground::StopEvidence>,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionFilters {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub task_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<String>,
}

impl SubscriptionFilters {
    pub(crate) fn matches(&self, event: &IpcEvent) -> bool {
        (self.task_ids.is_empty() || self.task_ids.contains(&event.task_id))
            && (self.kinds.is_empty() || self.kinds.contains(&event.kind))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum IpcResponse {
    Status {
        high_water_event_id: i64,
    },
    Stopping,
    SessionCreated {
        session_id: String,
        root_task_id: String,
    },
    AutonomousSessionCreated {
        session_id: String,
        root_task_id: String,
    },
    ApplicationRootAttached {
        session_id: String,
        root_task_id: String,
        message_capability: String,
        workspace: WorkerWorkspace,
    },
    ApplicationRootActivated,
    ApplicationRootDetached,
    /// The driver binding the daemon issued. The capability is narrow: it authorises
    /// activity writes for this epoch only, and is never shown to the UI.
    ForegroundDriverBound {
        binding: yi_agent_lifecycle::foreground::DriverBinding,
    },
    /// The activity snapshot after the report was applied (or the already-committed
    /// one, when the report was a duplicate).
    ForegroundActivity {
        snapshot: yi_agent_lifecycle::foreground::ActivitySnapshot,
    },
    /// The activity snapshot of a root that has never reported one.
    ForegroundActivityAbsent,
    /// The state of one recovery operation: preparing, blocked, ready or committed.
    ForegroundRecovery {
        status: yi_agent_lifecycle::foreground::RecoveryOperationStatus,
    },
    /// No recovery operation exists under that id.
    ForegroundRecoveryAbsent,
    /// A container exists at the given address. Carries no secret: this response
    /// only proves the attach succeeded, and a capability is issued later by
    /// `PrepareSessionDelegation` for a bound host.
    SessionContainerAttached {
        root: yi_agent_lifecycle::session_delegation::SessionRef,
        workspace: WorkerWorkspace,
    },
    SessionPrepared {
        binding: yi_agent_lifecycle::session_delegation::SessionBinding,
    },
    SessionTurnOpened {
        turn: yi_agent_lifecycle::session_delegation::SessionTurn,
    },
    SessionTurnClosed,
    /// The control stream is open; only this connection may send
    /// `SessionHostControlFrame`s after it.
    SessionHostControlReady,
    SessionHostExecutionRegistered {
        execution: yi_agent_lifecycle::foreground::ExecutionIdentity,
    },
    SessionHostStopRecorded,
    /// A structured delegation refusal, so the client branches on a code rather
    /// than on English text.
    SessionDelegationRejected {
        failure: yi_agent_lifecycle::session_delegation::SessionFailure,
    },
    /// A structured foreground refusal.
    ///
    /// This is a response rather than an `Error` because the client has to dispatch on
    /// `ForegroundErrorCode` and report exactly which executions blocked a stop. An
    /// `invalid_state` with a message would force the caller to match English text,
    /// which is the thing the design explicitly forbids.
    ForegroundFailure(yi_agent_lifecycle::foreground::ForegroundFailure),
    ScheduleCreated {
        schedule_id: String,
    },
    Schedules {
        schedules: Vec<IpcSchedule>,
    },
    ScheduleDeleted,
    TaskSpawned {
        task_id: String,
    },
    /// An upload is open and can accept chunks under `fork_token`.
    ForkUploadStarted {
        fork_token: String,
    },
    /// A chunk was accepted; `received` is the cumulative decoded byte count.
    ForkChunkAccepted {
        received: u64,
    },
    ForkUploadAborted,
    TaskStarted,
    TaskCancelled,
    /// The children a conversation-scoped cancellation actually cancelled.
    ///
    /// An empty `task_ids` is a success, not an error: the conversation may
    /// have finished everything already, and the caller is deleting anyway.
    ThreadTasksCancelled {
        task_ids: Vec<String>,
    },
    /// The conversation still owns live children and the caller did not force.
    ///
    /// Nothing was cancelled: this is a "please confirm" answer, not a failure,
    /// so it is a result the caller can render rather than an error it must
    /// catch. Re-issue the same request with `force` to proceed.
    ThreadHasActiveChildren {
        count: usize,
    },
    CancelPreview {
        confirmation_token: String,
        task_ids: Vec<String>,
        worktree_leases: Vec<String>,
        active_leases: Vec<IpcCancelLease>,
        unmerged_deliveries: Vec<IpcCancelDelivery>,
        expires_in_secs: u64,
    },
    TaskRetried,
    TaskPaused,
    TaskResumed,
    PermissionResolved,
    MessageQueued,
    WaitCompleted {
        status: String,
        children: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        reports: Vec<IpcCompletedChildReport>,
    },
    TaskDetail(IpcTaskDetail),
    TaskSummaries {
        tasks: Vec<IpcTaskSummary>,
    },
    /// A plugin's answer, passed through untouched.
    PluginResult {
        value: serde_json::Value,
    },
    /// 一个插件此刻的维护状态。
    ///
    /// `state` 是 `open` / `closed` / `unknown` 三者之一。调用方**必须**
    /// 把 `unknown` 当成"不能启动"：它表示权威记录读不出来，而不是"没有
    /// 维护"。
    PluginMaintenanceStatus {
        state: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        transaction_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        phase: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    TaskEvents {
        events: Vec<IpcEvent>,
    },
    TaskMailbox {
        messages: Vec<IpcMailboxMessage>,
    },
    TaskDiff {
        task_id: String,
        delivery_json: String,
        /// The child's real change as a unified diff, when it can be produced.
        /// Absent for a task that has not delivered, or whose worktree was
        /// already reclaimed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diff: Option<String>,
    },
    /// A point-in-time snapshot of one task's retained trace.
    TaskTrace {
        task_id: String,
        high_water_id: i64,
        rows: Vec<IpcTraceRow>,
    },
    Subscription(SubscriptionSnapshot),
    Event(IpcEvent),
    /// The wire payload for one followed trace row.
    TraceEvent(IpcTraceRow),
    /// The first frame of a trace subscription: the rows already durable at
    /// subscribe time, plus the cursor to resume from.
    TraceSubscription(TraceSnapshot),
    ResyncRequired,
    UnsupportedProtocol {
        supported_min: u32,
        supported_max: u32,
    },
    Error {
        code: IpcErrorCode,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcCancelLease {
    pub lease_id: String,
    pub task_id: String,
    pub resource_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcCancelDelivery {
    pub delivery_id: String,
    pub task_id: String,
    pub payload_json: String,
}

/// Stable, actor-free decision vocabulary for local daemon controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IpcPermissionDecision {
    Allow,
    Deny,
}

impl From<IpcPermissionDecision> for PermissionDecision {
    fn from(decision: IpcPermissionDecision) -> Self {
        match decision {
            IpcPermissionDecision::Allow => Self::Allow,
            IpcPermissionDecision::Deny => Self::Deny,
        }
    }
}

/// Stable public error categories for IPC consumers. Error responses never
/// serialize their underlying database, environment, or implementation detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IpcErrorCode {
    DaemonNotRunning,
    NotFound,
    InvalidState,
    AuthorityDenied,
    ConfirmationRequired,
    Conflict,
    Validation,
    RateLimited,
    Internal,
}

impl std::fmt::Display for IpcErrorCode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let code = match self {
            Self::DaemonNotRunning => "daemon_not_running",
            Self::NotFound => "not_found",
            Self::InvalidState => "invalid_state",
            Self::AuthorityDenied => "authority_denied",
            Self::ConfirmationRequired => "confirmation_required",
            Self::Conflict => "conflict",
            Self::Validation => "validation",
            Self::RateLimited => "rate_limited",
            Self::Internal => "internal",
        };
        formatter.write_str(code)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionSnapshot {
    pub high_water_event_id: i64,
    pub tasks: Vec<IpcTask>,
    pub events: Vec<IpcEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcTask {
    pub task_id: String,
    pub state: String,
}

/// 一个 task 的累计用量（同一 task 的所有 attempt 求和）。
///
/// 只带展示要用的三项：input / output / cache read。cache write 不在用户口径内，
/// 故既不记账也不传输。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcTaskUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcTaskSummary {
    pub task_id: String,
    pub state: String,
    pub is_root: bool,
    /// The task's parent, absent for a root task.
    pub parent_task_id: Option<String>,
    /// The conversation this task belongs to, when it was tagged with one.
    #[serde(default)]
    pub thread_id: Option<String>,
    /// 该 task 的累计用量。`serde(default)` 让不认识该字段的老客户端/老 daemon
    /// 双向兼容：老 daemon 不回时读出全零，而不是解析失败。
    #[serde(default)]
    pub usage: IpcTaskUsage,
    /// `execution` or `session_container`.
    ///
    /// A container is where a conversation delegates from, not an attempt at work.
    /// `serde(default)` keeps old peers readable: an absent role reads as an
    /// execution, which is what every row written before containers was.
    #[serde(default = "default_task_role")]
    pub role: String,
    /// `open` / `closed` for containers, `unknown` otherwise.
    ///
    /// Never derived from `state`: a container whose last attempt reads `stalled`
    /// is still open. `unknown` is returned as-is rather than filled from history.
    #[serde(default = "default_container_activity")]
    pub container_activity: String,
    /// A container's last attempt state, present only as history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub historical_execution_state: Option<String>,
}

fn default_task_role() -> String {
    "execution".to_string()
}

fn default_container_activity() -> String {
    "unknown".to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcSchedule {
    pub schedule_id: String,
    pub cron: String,
    pub objective: String,
    pub next_run_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcTaskDetail {
    pub task_id: String,
    pub session_id: String,
    pub parent_task_id: Option<String>,
    pub depth: u8,
    pub state: String,
    pub delivery_json: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_json: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcEvent {
    pub event_id: i64,
    pub task_id: String,
    pub kind: String,
    pub payload_json: String,
}

/// One durable observation row of a task's trace.
///
/// `event_id` is the row's own monotonic id and doubles as the resume cursor.
/// `kind` and `payload_json` are the writer's own vocabulary, returned verbatim:
/// the IPC layer never interprets a trace payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcTraceRow {
    pub event_id: i64,
    pub task_id: String,
    pub kind: String,
    pub payload_json: String,
}

/// The first frame of a trace subscription.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceSnapshot {
    pub high_water_id: i64,
    pub rows: Vec<IpcTraceRow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcMailboxMessage {
    pub message_id: String,
    pub recipient_task_id: String,
    pub sender_task_id: Option<String>,
    pub kind: String,
    pub priority: i64,
    pub payload_json: String,
    pub delivered_at: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcCompletedChildReport {
    pub task_id: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<String>,
    /// The child's delivered commit, when the child produced a delivery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<String>,
}

/// The stable wire payload for a top-level subscription event frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcEventPayload {
    pub task_id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub payload_json: String,
}

impl From<&IpcEvent> for IpcEventPayload {
    fn from(event: &IpcEvent) -> Self {
        Self {
            task_id: event.task_id.clone(),
            kind: event.kind.clone(),
            payload_json: event.payload_json.clone(),
        }
    }
}

pub(crate) fn response_envelope(
    request_id: &str,
    event_id: Option<i64>,
    response: IpcResponse,
) -> ResponseEnvelope {
    ResponseEnvelope {
        protocol_version: PROTOCOL_VERSION,
        request_id: request_id.into(),
        event_id,
        event: match &response {
            IpcResponse::Event(event) => Some(IpcEventPayload::from(event)),
            _ => None,
        },
        result: response,
    }
}

pub(crate) fn encode_envelope(envelope: &ResponseEnvelope) -> Result<Vec<u8>, IpcError> {
    let frame = serde_json::to_vec(envelope)?;
    if frame.len() <= MAX_FRAME_BYTES {
        return Ok(frame);
    }
    Err(IpcError::FrameTooLarge)
}

pub(crate) fn encode_subscription_envelope(
    envelope: &ResponseEnvelope,
) -> Result<(Vec<u8>, bool, bool), IpcError> {
    if let Ok(frame) = encode_envelope(envelope) {
        return Ok((
            frame,
            matches!(
                envelope.result,
                IpcResponse::Event(_) | IpcResponse::TraceEvent(_)
            ),
            matches!(envelope.result, IpcResponse::ResyncRequired),
        ));
    }
    Ok((
        serde_json::to_vec(&ResponseEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request_id: envelope.request_id.clone(),
            event_id: None,
            event: None,
            result: IpcResponse::ResyncRequired,
        })?,
        false,
        true,
    ))
}

pub(crate) fn next_request_id() -> String {
    format!(
        "request-{}-{}",
        std::process::id(),
        NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
    )
}

pub(crate) enum IncomingFrame {
    Complete(Vec<u8>),
    TooLarge(Vec<u8>),
}

/// Retain at most one frame prefix; oversized input is never buffered in full.
pub(crate) fn read_incoming_frame<R: BufRead>(
    reader: &mut R,
) -> Result<Option<IncomingFrame>, IpcError> {
    let mut prefix = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let remaining = (MAX_FRAME_BYTES + 1).saturating_sub(prefix.len());
        if remaining == 0 {
            return Ok(Some(IncomingFrame::TooLarge(prefix)));
        }
        match reader.take(remaining as u64).read_until(b'\n', &mut prefix) {
            Ok(0) if prefix.is_empty() => return Ok(None),
            Ok(0) => return Ok(Some(IncomingFrame::TooLarge(prefix))),
            Ok(_) if prefix.ends_with(b"\n") => {
                prefix.pop();
                return Ok(Some(IncomingFrame::Complete(prefix)));
            }
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(IpcError::Io(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "timed out reading IPC frame",
                    )));
                }
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => return Err(IpcError::Io(error)),
        }
    }
}

pub(crate) fn read_limited_frame<R: BufRead>(reader: &mut R) -> Result<Option<Vec<u8>>, IpcError> {
    let mut frame = Vec::new();
    let read = reader
        .take((MAX_FRAME_BYTES + 1) as u64)
        .read_until(b'\n', &mut frame)?;
    if read == 0 {
        return Ok(None);
    }
    if frame.len() > MAX_FRAME_BYTES {
        return Err(IpcError::FrameTooLarge);
    }
    if !frame.ends_with(b"\n") {
        return Err(IpcError::TruncatedFrame {
            received: frame.len(),
        });
    }
    frame.pop();
    Ok(Some(frame))
}

pub(crate) fn ipc_event(event: crate::repository::PersistedEvent) -> IpcEvent {
    IpcEvent {
        event_id: event.id,
        task_id: event.task_id.to_string(),
        kind: runtime_event_name(event.event).into(),
        payload_json: event.payload_json,
    }
}

pub(crate) fn ipc_trace_row(row: crate::repository::PersistedTraceRow) -> IpcTraceRow {
    IpcTraceRow {
        event_id: row.id,
        task_id: row.task_id.to_string(),
        kind: row.kind,
        payload_json: row.payload_json,
    }
}

pub(crate) fn runtime_event_name(event: crate::repository::RuntimeEvent) -> &'static str {
    match event {
        crate::repository::RuntimeEvent::RuntimeDraining => "runtime_draining",
        crate::repository::RuntimeEvent::RuntimeRecovered => "runtime_recovered",
        crate::repository::RuntimeEvent::TaskQueued => "task_queued",
        crate::repository::RuntimeEvent::TaskStarted => "task_started",
        crate::repository::RuntimeEvent::TaskCompleted => "task_completed",
        crate::repository::RuntimeEvent::TaskCancelled => "task_cancelled",
        crate::repository::RuntimeEvent::TaskPauseRequested => "task_pause_requested",
        crate::repository::RuntimeEvent::TaskPaused => "task_paused",
        crate::repository::RuntimeEvent::TaskProgress => "task_progress",
        crate::repository::RuntimeEvent::TaskBlocked => "task_blocked",
        crate::repository::RuntimeEvent::TaskStalled => "task_stalled",
        crate::repository::RuntimeEvent::TaskTimedOut => "task_timed_out",
        crate::repository::RuntimeEvent::TaskBudgetExhausted => "task_budget_exhausted",
        crate::repository::RuntimeEvent::TaskFailed => "task_failed",
        crate::repository::RuntimeEvent::TaskRecoveryRequired => "task_recovery_required",
        crate::repository::RuntimeEvent::TaskRecoveryAttested => "task_recovery_attested",
        crate::repository::RuntimeEvent::TaskDelivered => "task_delivered",
        crate::repository::RuntimeEvent::MailboxMessageQueued => "mailbox_message_queued",
        crate::repository::RuntimeEvent::MailboxMessageConsumed => "mailbox_message_consumed",
        crate::repository::RuntimeEvent::PermissionRequested => "permission_requested",
        crate::repository::RuntimeEvent::PermissionResolved => "permission_resolved",
    }
}

pub(crate) fn parse_id<T>(value: &str) -> Result<T, IpcError>
where
    T: FromStr,
{
    value.parse().map_err(|_| {
        IpcError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid ID",
        ))
    })
}

/// Resolves the requested workspace mode for a spawn request. An omitted mode
/// defaults to read-only so an existing client cannot accidentally request a
/// writable worktree; an explicit but unknown value is rejected.
pub(crate) fn parse_workspace_mode(
    mode: Option<String>,
) -> Result<yi_agent_core::ChildWriteMode, IpcError> {
    match mode.as_deref() {
        None => Ok(yi_agent_core::ChildWriteMode::ReadOnly),
        Some(value) => yi_agent_core::ChildWriteMode::parse(value).ok_or_else(|| {
            IpcError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("mode must be 'coding' or 'read_only', got {value}"),
            ))
        }),
    }
}

/// Resolves the optional inherited sandbox carried by a spawn request. An
/// omitted value means "no inheritance"; an explicit but unknown value is
/// rejected (`invalid_params` at the boundary).
pub(crate) fn parse_inherited_sandbox(
    sandbox: Option<String>,
) -> Result<Option<yi_agent_core::InheritedSandbox>, IpcError> {
    match sandbox.as_deref() {
        None => Ok(None),
        Some(value) => yi_agent_core::InheritedSandbox::parse(value)
            .map(Some)
            .ok_or_else(|| {
                IpcError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "sandbox must be 'read-only', 'workspace-write', or 'danger-full-access', got {value}"
                    ),
                ))
            }),
    }
}

#[cfg(test)]
mod plugin_protocol_tests {
    //! 插件与宿主的协议版本必须同步。
    //!
    //! 插件零 `yi-agent-*` 依赖，协议只能手写复刻，版本号也只能手抄。它曾经停在
    //! 1 而宿主已到 2：daemon 严格拒绝版本不匹配，于是"插件建会话"这条路一直是
    //! 断的（卡片永远停在 queued），而且不报错、只静默失败。
    //!
    //! 对照物是 `plugins/contracts/fixtures/ipc-protocol.json`——**公共夹具**，
    //! 不是某个插件的源码文件。主程序因此不依赖任何插件源码是否在树里：独立发布
    //! 之后宿主仍能验证自己与协议声明一致。

    use super::PROTOCOL_VERSION;

    const IPC_CONTRACT: &str =
        include_str!("../../../../../plugins/contracts/fixtures/ipc-protocol.json");

    fn declared_protocol_version() -> u32 {
        let contract: serde_json::Value =
            serde_json::from_str(IPC_CONTRACT).expect("契约必须是合法 JSON");
        contract["protocolVersion"]
            .as_u64()
            .expect("契约必须声明 protocolVersion") as u32
    }

    #[test]
    fn the_host_speaks_the_version_the_shared_contract_declares() {
        let declared = declared_protocol_version();
        assert_eq!(
            declared, PROTOCOL_VERSION,
            "契约说 {declared}、宿主说 {PROTOCOL_VERSION}：改协议就必须同时改契约，\
             否则插件与 daemon 会各自对着不同的数字"
        );
    }
}

#[cfg(test)]
mod inherited_sandbox_param_tests {
    use super::*;

    #[test]
    fn parse_inherited_sandbox_accepts_kebab_and_rejects_snake_case() {
        assert_eq!(parse_inherited_sandbox(None).unwrap(), None);
        assert_eq!(
            parse_inherited_sandbox(Some("danger-full-access".into())).unwrap(),
            Some(yi_agent_core::InheritedSandbox::DangerFullAccess)
        );
        assert_eq!(
            parse_inherited_sandbox(Some("workspace-write".into())).unwrap(),
            Some(yi_agent_core::InheritedSandbox::WorkspaceWrite)
        );
        assert_eq!(
            parse_inherited_sandbox(Some("read-only".into())).unwrap(),
            Some(yi_agent_core::InheritedSandbox::ReadOnly)
        );
        // snake_case must be rejected: the wire form is kebab-case.
        assert!(parse_inherited_sandbox(Some("read_only".into())).is_err());
        assert!(parse_inherited_sandbox(Some("bogus".into())).is_err());
    }
}

#[cfg(test)]
mod fork_wire_tests {
    use super::*;

    #[test]
    fn fork_upload_requests_round_trip_over_the_wire() {
        let request = IpcRequest::AppendForkChunk {
            fork_token: "t".into(),
            seq: 0,
            data: "aGk=".into(),
        };
        let encoded = serde_json::to_string(&request).unwrap();
        assert_eq!(
            serde_json::from_str::<IpcRequest>(&encoded).unwrap(),
            request
        );
    }

    #[test]
    fn an_older_spawn_request_without_fork_token_still_deserializes() {
        let json = r#"{"type":"SpawnApplicationChild","session_id":"s","parent_task_id":"p",
        "capability":"c","objective":"o"}"#;
        let parsed: IpcRequest = serde_json::from_str(json).unwrap();
        assert!(matches!(
            parsed,
            IpcRequest::SpawnApplicationChild {
                fork_token: None,
                ..
            }
        ));
    }
}

#[cfg(test)]
mod coding_requires_git_mapping_tests {
    use super::*;
    use crate::runtime::{CodingRequiresGitReason, RuntimeCoordinatorError};

    #[test]
    fn coding_requires_git_is_a_validation_error_with_a_stable_prefix() {
        let error = IpcError::Runtime(RuntimeCoordinatorError::CodingRequiresGit {
            workdir: std::path::PathBuf::from("/tmp/notes"),
            reason: CodingRequiresGitReason::NotARepository,
        });

        // The client must be able to tell "your environment is not ready" apart
        // from "the daemon is broken": Internal would send users chasing a
        // crashed daemon that is running fine.
        assert_eq!(ipc_error_code(&error), IpcErrorCode::Validation);

        let message = ipc_error_message(&error).expect("a validation rejection carries a message");
        assert!(
            message.starts_with(crate::runtime::CODING_REQUIRES_GIT_PREFIX),
            "the tool layer keys off this prefix, got {message:?}"
        );
        assert!(message.contains("not_a_repository"), "{message}");
        assert!(message.contains("/tmp/notes"), "{message}");
    }

    #[test]
    fn the_missing_commit_reason_is_distinguishable_at_the_wire() {
        let error = IpcError::Runtime(RuntimeCoordinatorError::CodingRequiresGit {
            workdir: std::path::PathBuf::from("/tmp/notes"),
            reason: CodingRequiresGitReason::NoInitialCommit,
        });
        let message = ipc_error_message(&error).expect("message");
        assert!(message.contains("no_initial_commit"), "{message}");
        assert!(
            !message.contains("not_a_repository"),
            "the two reasons must not be conflated: {message}"
        );
    }
}

#[cfg(test)]
mod settled_parent_wire_tests {
    use super::*;
    use crate::runtime::RuntimeCoordinatorError;

    /// The tool layer renders `daemon rejected {action}: {code}: {message}` and
    /// falls back to a bare code when there is no message. A settled-parent
    /// refusal is the caller's mistake with an actionable cause, so it has to
    /// reach the model as text, not as `invalid_state` alone.
    #[test]
    fn a_settled_parent_rejection_carries_a_readable_message() {
        let error = IpcError::Runtime(RuntimeCoordinatorError::ParentSettled {
            state: "completed".into(),
        });
        let message = ipc_error_message(&error)
            .expect("a settled-parent rejection must not reach the model as a bare code");
        assert!(message.contains("settled parent"), "{message}");
        assert!(message.contains("completed"), "{message}");
        assert_eq!(
            ipc_error_code(&error),
            IpcErrorCode::InvalidState,
            "a settled parent is an invalid-state refusal, not internal: {message}"
        );
    }
}
