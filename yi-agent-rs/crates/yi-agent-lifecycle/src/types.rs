//! Wire DTOs shared by every frontend, the guardian, and the daemon.
//!
//! Identities are strings on the wire, never PIDs: a PID is reusable and cannot
//! distinguish "the app that registered" from "a later process that inherited the
//! number". Secrets live in exactly one type ([`ClientGrant`]) and never appear in
//! `Debug` output or in a structured error.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// One client run, i.e. one App launch or one TUI process. Random, not derived
/// from a PID.
pub type ClientRunId = String;
/// One daemon start. Changes on every restart by construction.
pub type DaemonInstanceId = String;
/// Identity of the *daemon executable artifact*, not of the Tauri wrapper.
pub type BuildId = String;
pub type OperationId = String;
pub type ConfirmationToken = String;
pub type RootId = String;
pub type ManagedProcessId = String;

/// The runtime scope a client is talking to: the canonical runtime directory is
/// the identity, and the project/instance fields are assertions we re-check
/// rather than keys we trust.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeScope {
    pub key: String,
    pub runtime_dir: PathBuf,
    pub project_key: String,
    pub instance_home: PathBuf,
}

/// What a daemon executable *is*. Note `build_id` describes the daemon artifact,
/// which is deliberately independent of the wrapper version: a Tauri point release
/// that ships the same sidecar bytes must compare equal, and a rebuild with
/// different inputs at the same git SHA must compare different.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactIdentity {
    pub build_id: BuildId,
    pub display_version: String,
    pub executable: PathBuf,
    pub sha256: String,
    pub schema_min: u32,
    pub schema_max: u32,
    pub handover_version: u16,
    pub business_protocol_min: u32,
    pub business_protocol_max: u32,
    pub capabilities: Vec<String>,
}

/// What a *running* runtime reports about itself, read before any business request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeIdentity {
    pub scope_key: String,
    pub daemon_instance_id: Option<DaemonInstanceId>,
    pub guardian_build_id: Option<BuildId>,
    pub daemon_build_id: Option<BuildId>,
    pub mode: RuntimeMode,
    pub business_protocol: Option<u32>,
    pub daemon_display_version: Option<String>,
    pub guardian_display_version: Option<String>,
    pub capabilities: Vec<String>,
    pub schema_min: u32,
    pub schema_max: u32,
}

/// How the runtime was started. Only [`RuntimeMode::Managed`] carries the exit
/// guarantees; the other modes must be surfaced to the user as exceptions rather
/// than being silently treated as equivalent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeMode {
    Managed,
    LegacyIndependent,
    LegacyEmbedded,
    Unknown,
}

/// What kind of frontend holds a lease. There is deliberately no `Worker`
/// variant: an internal worker is not a frontend and must never keep a runtime
/// alive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientKind {
    App,
    Tui,
    Headless,
}

/// What a lease authorizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseMode {
    /// Full business authority.
    Business,
    /// Observability and upgrade control only; never authorizes a task.
    LeaseOnly,
    /// Registered during maintenance; authorized only by the next generation.
    WaitingNextGeneration,
}

/// The one type that carries a secret. `Debug` is hand-written to redact it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientGrant {
    pub client_run_id: ClientRunId,
    pub credential: String,
    pub epoch: u64,
    pub mode: LeaseMode,
}

impl std::fmt::Debug for ClientGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientGrant")
            .field("client_run_id", &self.client_run_id)
            .field("credential", &"<redacted>")
            .field("epoch", &self.epoch)
            .field("mode", &self.mode)
            .finish()
    }
}

/// Authority to attach one root in one daemon generation, issued by the guardian
/// only on an authenticated business lease.
///
/// It deliberately does *not* contain the lease secret: it cannot register or
/// renew, and a tool receives an even narrower task/spawn capability.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootAuthorization {
    pub scope_key: String,
    pub daemon_instance_id: String,
    pub client_run_id: String,
    pub root_id: String,
    pub capability: String,
}

impl std::fmt::Debug for RootAuthorization {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RootAuthorization")
            .field("scope_key", &self.scope_key)
            .field("daemon_instance_id", &self.daemon_instance_id)
            .field("client_run_id", &self.client_run_id)
            .field("root_id", &self.root_id)
            .field("capability", &"<redacted>")
            .finish()
    }
}

/// The switch/reclaim state machine's observable state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwitchPhase {
    Idle,
    Preparing,
    Draining,
    Stopping,
    Starting,
    Verifying,
    Ready,
    StoppedNoClients,
    Failed,
    ManualActionRequired,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationStatus {
    pub operation_id: OperationId,
    pub phase: SwitchPhase,
    pub detail: Option<String>,
    pub ready_identity: Option<RuntimeIdentity>,
}

/// Outcome of reclaiming one client's work.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReclaimResult {
    pub client_run_id: ClientRunId,
    pub stopped: bool,
    pub failures: Vec<String>,
}

/// Machine-readable failure taxonomy. `Unsupported` is a real answer, not a
/// success: an unknown operation must never be reported as accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlErrorCode {
    InvalidRequest,
    Unauthenticated,
    Unsupported,
    StaleEpoch,
    Released,
    VersionMismatch,
    Conflict,
    Internal,
}

/// How much a replacement would disturb, as counts only.
///
/// `complete` is the load-bearing field: `false` means the snapshot could not be
/// finished (the business daemon did not answer, or the read raced a change), so
/// every number here is a **lower bound**. A caller that treats `complete: false`
/// as "nothing is happening" would let a restart kill unseen work, so an
/// incomplete snapshot is always shown as unknown rather than as zero.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImpactCounts {
    /// Frontends currently registered (the App itself included).
    pub clients: u64,
    /// Clients that disconnected but may still reconnect inside their window.
    pub pending_clients: u64,
    /// Executions running right now.
    pub running: u64,
    /// Executions queued but not started.
    pub queued: u64,
    /// Shared plugin services with at least one owner.
    pub plugins: u64,
    /// Whether those numbers are a complete answer.
    pub complete: bool,
}

/// One affected thing, named so a human can judge it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImpactEntry {
    /// A stable, unbounded-set-free category (`client`, `execution`, `plugin`, ...).
    pub kind: String,
    /// The concrete id inside that category.
    pub id: String,
    /// Who it belongs to, when that is known. `None` is "unattributed", which is a
    /// reason to be more careful, not less.
    pub owner: Option<String>,
}

/// The two independent authorities whose state a preview describes.
///
/// Two counters rather than one derived hash: a hash could collide, and this value
/// decides whether a confirmation still applies. They are compared for equality,
/// never ordered -- an older generation is not "less" than a newer one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImpactRevision {
    /// The guardian's own counter: leases and process ownership.
    pub guardian: u64,
    /// The business daemon's counter. `None` means it did not answer: the impact
    /// is **unknown**, which must never be read as zero.
    pub coordinator: Option<u64>,
}

/// What a restart would disturb, plus the token that authorises consuming it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestartPreview {
    /// Identifies this snapshot for paging. Not a credential on its own: every
    /// page request is still checked against the caller's grant and context.
    pub snapshot_id: String,
    /// The one-shot token a confirmation must present. Empty on a page after the
    /// first, which is what makes "paging cannot confirm" structural.
    pub token: ConfirmationToken,
    pub revision: ImpactRevision,
    pub counts: ImpactCounts,
    pub entries: Vec<ImpactEntry>,
    /// Set when more entries exist than fit in one frame.
    pub next_cursor: Option<String>,
    /// True when the action is not the harmless case: someone else, some running
    /// work, or an unknown impact. The UI must ask a second time.
    pub requires_second_confirmation: bool,
    /// Whether a restart may be attempted at all. `false` with a `blocked_reason`
    /// is an answer, not a failure: some runtimes cannot be upgraded automatically.
    pub can_restart: bool,
    pub blocked_reason: Option<String>,
}

/// Everything a confirmation is bound to, so a stale click cannot do new damage.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfirmationContext {
    pub scope_key: String,
    pub daemon_instance_id: String,
    pub target: ArtifactIdentity,
    pub revision: ImpactRevision,
}

/// One connected client, as a status reader may see it.
///
/// Deliberately has no credential, no epoch and no message text: the App needs to
/// tell a user *who* is keeping a runtime alive ("this App, 2 TUI"), and nothing
/// beyond that belongs in a read that a UI polls.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientSummary {
    pub client_run_id: ClientRunId,
    pub kind: ClientKind,
    pub mode: LeaseMode,
}

/// The guardian's own lightweight status answer.
///
/// This is the *shared* source both the app-server (`runtime/status`) and the
/// desktop shell read, so the two cannot describe one runtime differently.
///
/// `counts` is always whole-runtime while `clients` is one page: a UI that showed
/// only the page's length would say "1 client" to a runtime with twelve. That is
/// why the two are separate fields rather than "the page is the answer".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeStatusSnapshot {
    pub identity: RuntimeIdentity,
    /// What the guardian can verify about its own impact, without asking the
    /// daemon. `complete` stays `false` here: the daemon's executor and plugin
    /// rows can only be added by a preview, and a status read must not block on a
    /// wedged business socket.
    pub counts: ImpactCounts,
    /// This page of clients.
    pub clients: Vec<ClientSummary>,
    /// Present when more clients exist beyond this page.
    pub clients_next_cursor: Option<String>,
    /// The switch this runtime is running, if any.
    pub active_operation: Option<OperationStatus>,
    /// Whether this build can perform a switch at all.
    pub local_restart_available: bool,
}

/// Requests on the control channel (protocol v1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlRequest {
    /// Read-only identity. Never registers, never starts anything.
    Identity,
    Register {
        client_run_id: ClientRunId,
        kind: ClientKind,
        mode: LeaseMode,
    },
    Resume {
        grant: ClientGrant,
    },
    Heartbeat {
        grant: ClientGrant,
    },
    Release {
        grant: ClientGrant,
    },
    ReclaimStatus {
        grant: ClientGrant,
        operation_id: OperationId,
    },
    Operation {
        grant: ClientGrant,
        operation_id: OperationId,
    },
    /// Ask whether one execution may start.
    ///
    /// Owner-bound by construction: the grant must match the live lease, so this is
    /// not a way to learn about, or act for, another client. It cannot spawn
    /// anything -- a refusal is the only power it has.
    Admission {
        asked: ExecutionAdmission,
    },
    /// Read-only impact preview. Never drains, never takes a maintenance lock.
    ///
    /// `target` is the artifact the caller wants to move to; it is part of the
    /// token binding because "upgrade to 9.2.2" and "upgrade to 9.3.0" are
    /// different decisions even when the current impact is identical.
    Preview {
        grant: ClientGrant,
        target: ArtifactIdentity,
    },
    /// One more page of an existing snapshot. Cannot confirm anything.
    PreviewPage {
        grant: ClientGrant,
        snapshot_id: String,
        cursor: String,
    },
    /// Consume a preview token. `operation_id` is chosen by the caller *before*
    /// the click so a lost response does not require a second confirmation: the
    /// same id is queried instead of the action being repeated.
    Confirm {
        grant: ClientGrant,
        token: ConfirmationToken,
        operation_id: OperationId,
    },
    /// Asks for a fresh private descriptor to start tool processes on.
    ///
    /// This is the one control operation whose *answer* is a descriptor rather
    /// than a frame: the guardian creates the pair, keeps one end and hands the
    /// other back over `SCM_RIGHTS`. It is deliberately not a socket path a tool
    /// could dial by itself -- the owner of a channel is then a fact of who the
    /// guardian gave it to, not a string in a request.
    ///
    /// `grant` is what makes it owner-bound. A lease-only observer cannot ask:
    /// observers never authorize a task, and a tool process is work. The guardian
    /// answers `Unsupported` when it holds no process registry, because a channel
    /// with nothing behind it would accept a spawn it could never reclaim.
    OpenManagedChannel {
        grant: ClientGrant,
    },
    /// Read-only status: identity, whole-runtime counts, one page of clients, and
    /// any switch in flight.
    ///
    /// Deliberately **grant-free**, and that is not an oversight:
    ///
    /// * It must not create or renew a lease. A UI polls this continuously, and if
    ///   the poll were what kept a runtime alive, a user who walked away from a
    ///   window would keep a daemon running forever.
    /// * The app-server has to answer it too, and the app-server is forbidden to
    ///   hold the App's renewable credential (see `shell_authorization`). Requiring
    ///   a grant would leave the only legitimate local reader unable to ask.
    /// * It carries no secret: client run ids are names, and the credential lives
    ///   in `ClientGrant` alone. The access boundary is the socket itself (0600
    ///   inside a 0700 directory), not this frame.
    Status {
        /// Opaque `client_run_id` to resume the client page after.
        ///
        /// A cursor rather than an offset because offsets shift under insertion:
        /// a client registering between two page reads would make page 2 repeat or
        /// skip a row.
        cursor: Option<String>,
    },
    /// Ask the guardian for authority over one foreground root.
    ///
    /// This is a *host* operation. It is never registered as a model tool and never
    /// reachable from the generic JSON-RPC surface: the capability it returns is the
    /// only thing that may bind a driver, write activity or start a tool under that
    /// root, so a model that could ask for one could grant itself the exemption.
    ///
    /// `grant` proves which client is asking. The guardian refuses unless that client
    /// holds a live `Business` lease, so a `LeaseOnly` observer cannot mint authority.
    /// `intent.thread_key` is never authority by itself: it is a lookup key, and the
    /// answer carries a capability that cannot be derived from it.
    AuthorizeRoot {
        grant: ClientGrant,
        intent: crate::foreground::RootBindIntent,
    },
    /// Ask the daemon to prepare a recovery of a root whose attempt was settled.
    ///
    /// Same shape as [`Self::AuthorizeRoot`]: the guardian verifies the lease and its
    /// own view of the root before the request reaches the daemon. The daemon then
    /// does the authorize-and-freeze half, which is where the transaction boundary
    /// lives.
    PrepareRootRecovery {
        grant: ClientGrant,
        request: crate::foreground::RecoveryRequest,
    },
    /// Confirm a prepared recovery with the token the prepare returned.
    ConfirmRootRecovery {
        grant: ClientGrant,
        request: crate::foreground::RecoveryRequest,
        token: crate::foreground::SecretCapability,
    },
    /// Read the stored status of one recovery operation.
    ///
    /// Grant-bearing like the rest: a read that answered for any root would let a
    /// client enumerate sessions it does not own.
    RootRecoveryOperation {
        grant: ClientGrant,
        root: crate::foreground::RootRef,
        operation_id: String,
    },
    /// Read what a recovery of this root would stop, without freezing anything.
    ///
    /// Split from [`Self::PrepareRootRecovery`] because looking must not stop work:
    /// a user who opens the recovery panel and then walks away has not asked for
    /// their conversation to be halted. `expected` is the fence the host believes is
    /// current; the daemon refuses a stale one rather than describing another attempt.
    InspectRootRecovery {
        grant: ClientGrant,
        expected: crate::foreground::ForegroundFence,
    },
}

/// A narrow, owner-bound execution-admission question.
///
/// This is *not* a public control operation: it is asked over a private descriptor
/// the guardian issued to one owner, and it can only ever answer about work that
/// owner is about to start. It grants no ability to spawn, renew, or register --
/// its whole purpose is to let the guardian refuse a Start after it has begun
/// withdrawing a client.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionAdmission {
    /// The owner's own grant. Proves the asker really speaks for this client:
    /// without it a caller could ask about someone else's owner, or invent one.
    pub grant: ClientGrant,
    pub generation: String,
    pub task: String,
    pub attempt: String,
}

/// The guardian's answer to an [`ExecutionAdmission`] question.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdmissionVerdict {
    /// This exact execution may start now.
    Admitted,
    /// The owner's admission is closed: released, expired, or past this
    /// generation. The caller must not start the work.
    Refused { reason: String },
}

/// Responses on the control channel (protocol v1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlResponse {
    Identity(RuntimeIdentity),
    /// A granted lease, on register or resume.
    Granted(ClientGrant),
    /// Generic acceptance of a state-changing request with nothing to return.
    Ack,
    /// The release was accepted and its reclaim work is now running.
    ReleaseStarted {
        operation_id: OperationId,
    },
    Operation(Option<OperationStatus>),
    ReclaimStatus {
        operation_id: OperationId,
        result: Option<ReclaimResult>,
    },
    Rejected {
        code: ControlErrorCode,
        reason: String,
    },
    /// This endpoint does not implement the request at all.
    Unsupported {
        detail: String,
    },
    /// The snapshot (first page, or a later page with an empty token).
    Preview(RestartPreview),
    /// The confirmation was accepted and bound to `operation_id`.
    Confirmed {
        operation_id: OperationId,
    },
    /// The answer to [`ControlRequest::Status`].
    ///
    /// Boxed because a status snapshot (identity + counts + a page of clients) is
    /// an order of magnitude larger than the smallest reply on this channel, and
    /// every reply would otherwise be sized for the biggest one. The frame is
    /// encoded and dropped immediately, so one indirection costs nothing.
    Status(Box<RuntimeStatusSnapshot>),
    /// The answer to [`ControlRequest::AuthorizeRoot`]: authority for this client to
    /// bind, and nothing else. It cannot register, renew, or act for another client.
    RootAuthorized(RootAuthorization),
    /// The answer to the recovery requests. `None` means the daemon has no record of
    /// that operation, which is *not* the same as "no recovery happened".
    RootRecovery(Option<crate::foreground::RecoveryOperationStatus>),
    /// The answer to [`ControlRequest::InspectRootRecovery`]: what a freeze would stop.
    ///
    /// Its own variant rather than a [`crate::foreground::RecoveryOperationStatus`]
    /// because an impact is not a status: `Blocked` names a *reason a token is
    /// withheld*, while an impact is what the user reads before deciding whether to
    /// ask for one.
    RootRecoveryImpact(crate::foreground::RecoveryImpact),
    /// The private channel was created; the descriptor travels beside this frame.
    ///
    /// Carrying the owner back is not a convenience: the client must build the
    /// spawn policy from *this* answer rather than from the id it asked about, so a
    /// guardian that granted a different owner cannot be silently mislabelled.
    ChannelOpened {
        /// The owner the guardian actually bound the channel to.
        owner: String,
        /// The runtime generation the channel belongs to.
        generation: String,
    },
}

/// A portable description of a bundled daemon artifact, emitted at build time and
/// resolved against the real bundle at runtime.
///
/// Paths are relative to the bundle root: a manifest that embedded the build
/// machine's absolute path would be wrong the moment the app is installed
/// elsewhere. It also carries no sign-time digest from the build machine — the
/// final digest is written after signing by `finalize-runtime-bundle.sh`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundledArtifactManifest {
    pub build_id: BuildId,
    pub display_version: String,
    /// Relative to the bundle root, always forward-slash separated.
    pub executable: String,
    pub sha256: String,
    pub schema_min: u32,
    pub schema_max: u32,
    pub handover_version: u16,
    pub business_protocol_min: u32,
    pub business_protocol_max: u32,
    pub capabilities: Vec<String>,
}

/// One process the guardian is asked to start on a frontend's behalf.
///
/// The fields deliberately stop short of anything that changes *who* the process
/// is: no owner, no capability, no generation. Those come from the private
/// descriptor the request arrives on, so a caller cannot ask to run as somebody
/// else.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedSpawnSpec {
    /// Absolute path. A relative or empty program would be resolved against
    /// whatever cwd the guardian happens to have, which is not the caller's.
    pub executable: PathBuf,
    pub args: Vec<String>,
    /// Environment to *add* or override. The guardian's own environment is not
    /// inherited wholesale: a provider key in the caller's environment has no
    /// business reaching a tool process unless it was passed explicitly.
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
}

/// `Debug` prints argument count and environment keys, never values.
///
/// A spawn spec routinely carries credentials in `env`, and a trace line is not
/// the place to persist them.
impl std::fmt::Debug for ManagedSpawnSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagedSpawnSpec")
            .field("executable", &self.executable)
            .field("arg_count", &self.args.len())
            .field(
                "env_keys",
                &self.env.iter().map(|(key, _)| key).collect::<Vec<_>>(),
            )
            .field("cwd", &self.cwd)
            .finish()
    }
}

/// What the guardian hands back after it registered the process.
///
/// `pid`/`pgid` are for diagnosis and for a caller that needs to name the group
/// in a log. Control authority stays with the guardian: holding these numbers
/// does not let anyone signal the group behind its back.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedSpawnReceipt {
    pub process_id: ManagedProcessId,
    pub pid: u32,
    pub pgid: i32,
}

/// How a managed process ended.
///
/// `group_stopped` is what makes this usable as a stop proof: `code` describes
/// the leader, and a leader can exit while a child it forked keeps running.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedExit {
    /// Exit code, when the leader exited normally.
    pub code: Option<i32>,
    /// Terminating signal, when the leader was signalled.
    pub signal: Option<i32>,
    /// Whether the whole group was observed empty.
    pub group_stopped: bool,
}

/// One request on a private managed-process channel.
///
/// Frame-per-request and blocking, like the control protocol: these carry
/// descriptors, so they travel on their own connection rather than through the
/// public 16 KiB control frames.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ManagedSpawnRequest {
    /// Start one group. The three standard descriptors arrive out of band, as
    /// `SCM_RIGHTS` ancillary data, so they are not part of this frame.
    Spawn(ManagedSpawnSpec),
    /// Wait for the group to end, within the caller's remaining budget.
    ///
    /// `budget_secs` is the caller's own wait budget, and it has to travel with
    /// the request: the channel's read timeout exists to notice a dead peer, not
    /// to bound a legitimate wait. Serving `Wait` with the channel budget meant a
    /// command that ran longer than that would be answered with the status read
    /// at that moment (`group_stopped: false`, no exit code) while its process
    /// group was still alive -- indistinguishable from "the command failed".
    /// Zero means the caller sent no budget (an older build); the server then
    /// falls back to the channel budget, which is the old behaviour.
    Wait {
        process_id: ManagedProcessId,
        #[serde(default)]
        budget_secs: u64,
    },
    /// Ask without waiting.
    TryWait { process_id: ManagedProcessId },
    /// Terminate the group and report what happened.
    Terminate { process_id: ManagedProcessId },
}

impl std::fmt::Debug for ManagedSpawnRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(spec) => f.debug_tuple("Spawn").field(spec).finish(),
            Self::Wait {
                process_id,
                budget_secs,
            } => f
                .debug_tuple("Wait")
                .field(process_id)
                .field(budget_secs)
                .finish(),
            Self::TryWait { process_id } => f.debug_tuple("TryWait").field(process_id).finish(),
            Self::Terminate { process_id } => f.debug_tuple("Terminate").field(process_id).finish(),
        }
    }
}

/// The guardian's answer on a managed-process channel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ManagedSpawnResponse {
    Spawned(ManagedSpawnReceipt),
    Exit(ManagedExit),
    /// Still running; the caller may ask again.
    Running,
    Failed {
        reason: String,
    },
}
