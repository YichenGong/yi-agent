//! The host's half of the foreground driver contract.
//!
//! Three applications drive a foreground root — the TUI, the desktop app-server and a
//! headless run — so the client lives here rather than in any of them. Kanban gets no
//! special branch: it uses the same driver, and a divergence between the three would be
//! a divergence in the contract.
//!
//! Two rules shape this module:
//!
//! * **The driver owns the sequence.** The daemon answers a bind with `last_seq`, and
//!   every later report is `last_seq + 1`. A client that restarted, or that reconnected
//!   after losing an answer, therefore never has to guess where the numbering was — and
//!   never silently rewinds a snapshot the daemon already committed.
//! * **A failed report is not a failed turn.** An activity write that returns a transport
//!   error is kept as a *pending* report with its original sequence and payload, so the
//!   retry is a replay rather than a new statement. Nothing here retries by itself: an
//!   ambiguous failure means "the daemon may already have applied this", and only the
//!   caller knows whether the turn is still worth describing.
//!
//! Activity is deliberately not a heartbeat. Nothing in this module renews a lease,
//! extends a deadline, or cancels a child; a report only says what the conversation is
//! doing right now.

use std::sync::Arc;

use yi_agent_lifecycle::foreground::{
    ActivityPhase, ActivitySnapshot, ActivityUpdate, DriverBinding, ForegroundErrorCode,
    ForegroundFailure, RootRef, SecretCapability,
};
use yi_agent_lifecycle::types::RootAuthorization;
use yi_agent_store::ipc::{IpcRequest, IpcResponse};

use crate::thread_root::{RetryPolicy, ThreadRoot};

/// Why a driver operation did not produce a committed snapshot.
///
/// The split is load-bearing: `Rejected` is a policy answer the caller may act on
/// (`stale_driver` means re-bind, `admission_closed` means stop trying), while
/// `Transport` and `Protocol` mean the conversation's own state is unchanged and the
/// answer is unknown. Collapsing them would make every network hiccup look like a
/// refusal, and every refusal look retryable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForegroundClientError {
    Rejected(ForegroundFailure),
    Transport(String),
    Protocol(String),
}

impl std::fmt::Display for ForegroundClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(failure) => write!(
                formatter,
                "the daemon refused the foreground operation: {:?} ({})",
                failure.code, failure.reason
            ),
            Self::Transport(message) => write!(formatter, "foreground transport: {message}"),
            Self::Protocol(message) => write!(formatter, "foreground protocol: {message}"),
        }
    }
}

impl std::error::Error for ForegroundClientError {}

/// A report the daemon has not confirmed, kept so a retry can replay it verbatim.
///
/// Same sequence and same payload on purpose: bumping either on retry would either
/// conflict with a report the daemon did apply, or look like a new statement about a
/// turn that has already moved on.
#[derive(Debug, Clone)]
struct PendingReport {
    update: ActivityUpdate,
}

/// The state machine behind [`ForegroundDriver`].
///
/// Split out so the ordering rules can be tested without a daemon: the transport is a
/// trait, and the fake in this module's tests records exactly what was sent. An
/// integration test that needed a live daemon to observe "a late finish is refused
/// locally" would be testing the daemon, not this rule.
pub(crate) struct DriverState<T: ForegroundTransport> {
    transport: T,
    authorization: RootAuthorization,
    binding: DriverBinding,
    /// The last snapshot the daemon confirmed. Starts at the binding's `last_seq` with
    /// an `idle` phase, because that is what the daemon's row says before any report.
    committed: ActivityUpdate,
    pending: Option<PendingReport>,
}

impl<T: ForegroundTransport> DriverState<T> {
    fn new(transport: T, authorization: RootAuthorization, binding: DriverBinding) -> Self {
        let committed = ActivityUpdate {
            fence: binding.fence.clone(),
            turn_id: None,
            seq: binding.last_seq,
            phase: ActivityPhase::Idle,
        };
        Self {
            transport,
            authorization,
            binding,
            committed,
            pending: None,
        }
    }

    fn last_snapshot(&self) -> &ActivityUpdate {
        &self.committed
    }

    fn open_turn(&mut self, turn_id: &str) -> Result<(), ForegroundClientError> {
        if turn_id.trim().is_empty() {
            return Err(ForegroundClientError::Protocol(
                "a turn id is required to open a turn".into(),
            ));
        }
        if self.committed.phase != ActivityPhase::Idle || self.committed.turn_id.is_some() {
            // One turn at a time: opening a second while one is live would make
            // "which turn did this finish?" ambiguous at the daemon, and the daemon
            // would have to reject it anyway.
            return Err(ForegroundClientError::Protocol(format!(
                "turn {} is still open; a driver opens one turn at a time",
                self.committed.turn_id.as_deref().unwrap_or("<unnamed>")
            )));
        }
        self.report(Some(turn_id), ActivityPhase::Running)
    }

    fn report(
        &mut self,
        turn_id: Option<&str>,
        phase: ActivityPhase,
    ) -> Result<(), ForegroundClientError> {
        if phase == ActivityPhase::Unknown {
            // Only the daemon produces `unknown` (a driver is gone, so it cannot report
            // that it does not know). Sending one would let a confused client erase what
            // the conversation was doing.
            return Err(ForegroundClientError::Protocol(
                "a driver cannot report the unknown phase".into(),
            ));
        }
        // The turn rules are mirrored here, not invented: the daemon's reducer refuses
        // exactly these shapes, and sending one anyway would spend a sequence number on
        // a statement already known to be wrong.
        match (self.committed.turn_id.as_deref(), turn_id, phase) {
            (None, _, ActivityPhase::WaitingForChildren | ActivityPhase::WaitingForPermission) => {
                return Err(ForegroundClientError::Protocol(
                    "a waiting phase is only meaningful inside an open turn".into(),
                ));
            }
            // `running` without a turn is the one shape `open_turn` exists to prevent.
            (None, None, ActivityPhase::Running) => {
                return Err(ForegroundClientError::Protocol(
                    "running requires a turn id".into(),
                ));
            }
            // Inside a turn every report names the current turn -- including the `idle`
            // that closes it. The daemon's reducer refuses an unnamed `idle` there, so a
            // client that sent one would have its finish rejected and would then retry
            // forever with an unconfirmed report.
            (Some(_), Some(named), _) if Some(named) == self.committed.turn_id.as_deref() => {}
            (Some(open), other, _) => {
                return Err(ForegroundClientError::Protocol(format!(
                    "the report names {other:?} but {open} is the open turn"
                )));
            }
            _ => {}
        }
        if self.pending.is_some() {
            // The previous report's outcome is unknown. Sending a new sequence on top
            // would either lose it or conflict with it, so the caller has to resolve the
            // pending one first (`retry_pending`).
            return Err(ForegroundClientError::Protocol(
                "the previous activity report is still unconfirmed".into(),
            ));
        }
        let next = self.committed.seq.saturating_add(1);
        let update = ActivityUpdate {
            fence: self.binding.fence.clone(),
            turn_id: turn_id.map(str::to_owned),
            seq: next,
            phase,
        };
        self.send(update)
    }

    /// Records a daemon-confirmed view.
    ///
    /// One normalisation, in one place: a closed turn leaves no `turn_id` behind. The
    /// daemon keeps the id for de-duplication, but this side must not, or the next
    /// `open_turn` would believe a turn is still live and `finish` would look for one
    /// that has already ended.
    fn commit(&mut self, mut update: ActivityUpdate) {
        if update.phase == ActivityPhase::Idle {
            update.turn_id = None;
        }
        self.committed = update;
    }

    /// Sends one report and advances the committed state only if the daemon confirmed it.
    fn send(&mut self, update: ActivityUpdate) -> Result<(), ForegroundClientError> {
        match self
            .transport
            .set_activity(&self.binding.driver_capability, &update)
        {
            Ok(snapshot) => {
                self.commit(snapshot.update);
                self.pending = None;
                Ok(())
            }
            Err(error @ ForegroundClientError::Transport(_)) => {
                // Keep it as pending: the daemon may or may not have applied it, so the
                // only safe retry is the same sequence with the same payload.
                self.pending = Some(PendingReport { update });
                Err(error)
            }
            // A refusal means nothing was written, so the pending slot stays empty and
            // the next report may reuse the sequence.
            Err(error) => Err(error),
        }
    }

    fn finish(&mut self, turn_id: &str) -> Result<(), ForegroundClientError> {
        let Some(current) = self.committed.turn_id.as_deref() else {
            return Err(ForegroundClientError::Protocol(
                "no turn is open, so none can be finished".into(),
            ));
        };
        if current != turn_id {
            // Refused locally: the daemon's fence would reject it, and sending it anyway
            // would spend a sequence number on a statement known to be wrong.
            return Err(ForegroundClientError::Protocol(format!(
                "turn {turn_id} is not the open turn ({current})"
            )));
        }
        // Named, not anonymous: the daemon only accepts a finish that identifies the
        // turn being closed, so `None` would be rejected as an illegal transition.
        self.report(Some(turn_id), ActivityPhase::Idle)
    }

    /// Reads the daemon's committed snapshot and adopts it when it is about this epoch.
    fn refresh(&mut self) -> Result<ActivitySnapshot, ForegroundClientError> {
        let authorization = self.authorization.clone();
        let root: RootRef = self.binding.fence.root.clone();
        let response = self.transport.read_activity(&authorization, &root)?;
        match response {
            IpcResponse::ForegroundActivity { snapshot } => {
                // Only adopt the daemon's view when it is about the same epoch: a
                // snapshot from a newer driver belongs to a driver that replaced this
                // one, and adopting it would let this process speak for that driver.
                if snapshot.update.fence.driver_epoch == self.binding.fence.driver_epoch {
                    let update = snapshot.update.clone();
                    self.commit(update);
                }
                Ok(snapshot)
            }
            IpcResponse::ForegroundActivityAbsent => Err(ForegroundClientError::Protocol(
                "the daemon has no activity for this root".into(),
            )),
            other => Err(rejection(other)?),
        }
    }

    /// Replays the unconfirmed report with its original sequence and payload.
    ///
    /// Returns `Ok(())` when there was nothing pending, which makes "resolve the pending
    /// report" safe to call unconditionally after a reconnect.
    fn retry_pending(&mut self) -> Result<(), ForegroundClientError> {
        let Some(pending) = self.pending.clone() else {
            return Ok(());
        };
        match self
            .transport
            .set_activity(&self.binding.driver_capability, &pending.update)
        {
            Ok(snapshot) => {
                self.commit(snapshot.update);
                self.pending = None;
                Ok(())
            }
            Err(error @ ForegroundClientError::Transport(_)) => {
                self.pending = Some(pending);
                Err(error)
            }
            Err(error) => Err(error),
        }
    }
}

/// The subset of the socket protocol a driver needs.
///
/// A trait rather than direct `send_request` calls so the ordering state machine above
/// has a fake; production uses [`SocketTransport`], which goes through
/// [`ThreadRoot::send_as_root`] and therefore inherits its liveness handling.
pub trait ForegroundTransport {
    fn bind(
        &self,
        authorization: &RootAuthorization,
        operation_id: &str,
    ) -> Result<DriverBinding, ForegroundClientError>;

    fn set_activity(
        &self,
        capability: &SecretCapability,
        update: &ActivityUpdate,
    ) -> Result<ActivitySnapshot, ForegroundClientError>;

    fn read_activity(
        &self,
        authorization: &RootAuthorization,
        root: &RootRef,
    ) -> Result<IpcResponse, ForegroundClientError>;
}

/// The real transport: one attached conversation root, over the daemon socket.
#[allow(dead_code)]
pub struct SocketTransport {
    root: Arc<ThreadRoot>,
}

impl ForegroundTransport for SocketTransport {
    fn bind(
        &self,
        authorization: &RootAuthorization,
        operation_id: &str,
    ) -> Result<DriverBinding, ForegroundClientError> {
        let authorization = authorization.clone();
        let operation_id = operation_id.to_string();
        // `Idempotent`: a bind carries an operation id, so replaying it after an
        // ambiguous transport failure returns the same epoch instead of minting a
        // second driver. That is exactly the case the policy exists for.
        let response = self
            .root
            // `send_as_root` takes `Fn`, so the payload is rebuilt per attempt: the
            // second attempt must carry the same operation id, which is the whole point.
            .send_as_root(RetryPolicy::Idempotent, move |_handle| {
                IpcRequest::BindApplicationRootDriver {
                    authorization: authorization.clone(),
                    operation_id: operation_id.clone(),
                }
            })
            .map_err(ForegroundClientError::Transport)?;
        match response {
            IpcResponse::ForegroundDriverBound { binding } => Ok(binding),
            other => Err(rejection(other)?),
        }
    }

    fn read_activity(
        &self,
        authorization: &RootAuthorization,
        root: &RootRef,
    ) -> Result<IpcResponse, ForegroundClientError> {
        let authorization = authorization.clone();
        let root = root.clone();
        self.root
            .send_as_root(RetryPolicy::Idempotent, move |_handle| {
                IpcRequest::GetApplicationRootActivity {
                    authorization: authorization.clone(),
                    root: root.clone(),
                }
            })
            .map_err(ForegroundClientError::Transport)
    }

    fn set_activity(
        &self,
        capability: &SecretCapability,
        update: &ActivityUpdate,
    ) -> Result<ActivitySnapshot, ForegroundClientError> {
        let capability = capability.clone();
        let update = update.clone();
        // `Idempotent` for the same reason, one level down: the daemon reduces a
        // replayed (seq, payload) pair to `Duplicate` instead of applying it twice.
        let response = self
            .root
            .send_as_root(RetryPolicy::Idempotent, move |_handle| {
                IpcRequest::SetApplicationRootActivity {
                    driver_capability: capability.clone(),
                    update: update.clone(),
                }
            })
            .map_err(ForegroundClientError::Transport)?;
        match response {
            IpcResponse::ForegroundActivity { snapshot } => Ok(snapshot),
            other => Err(rejection(other)?),
        }
    }
}

/// Turns a non-foreground answer into the client's error vocabulary.
///
/// Two distinctions matter. A structured `ForegroundFailure` is a policy answer and
/// becomes `Rejected`. A plain `Error` is only a rejection when its code is one the
/// foreground contract uses (`authority_denied`, `invalid_state`); anything else means
/// the two ends disagree about the protocol, which must not look like a refusal.
fn rejection(response: IpcResponse) -> Result<ForegroundClientError, ForegroundClientError> {
    match response {
        IpcResponse::ForegroundFailure(failure) => Err(ForegroundClientError::Rejected(failure)),
        IpcResponse::Error { code, message } => {
            // Only the codes that mean exactly one thing are mapped. `InvalidState` is
            // deliberately not one of them: it covers "admission is closed", "this root
            // is frozen" and "the binding is corrupt", and a client that reported the
            // first two as a corrupt binding would send its host into a recovery path it
            // does not need. Unmapped refusals stay `Protocol` so the caller can see the
            // daemon's own words instead of a guess.
            let rejected = match code {
                yi_agent_store::ipc::IpcErrorCode::AuthorityDenied => {
                    Some(ForegroundErrorCode::AuthorityDenied)
                }
                _ => None,
            };
            match rejected {
                Some(code) => Err(ForegroundClientError::Rejected(ForegroundFailure::new(
                    code,
                    message.unwrap_or_else(|| "the daemon refused the request".into()),
                ))),
                None => Err(ForegroundClientError::Protocol(format!(
                    "unexpected response for a foreground request: {code}"
                ))),
            }
        }
        other => Err(ForegroundClientError::Protocol(format!(
            "unexpected response for a foreground request: {other:?}"
        ))),
    }
}

/// The host's driver handle for one conversation root.
///
/// Held for the lifetime of the host's foreground work. Dropping it does not tell the
/// daemon anything: an abandoned driver is discovered by the bridge/owner lifecycle, not
/// by a farewell message that a killed process would never send.
pub struct ForegroundDriver {
    state: DriverState<SocketTransport>,
}

impl ForegroundDriver {
    /// Binds this host as the root's driver, or replays an existing binding.
    ///
    /// `operation_id` identifies *this bind attempt*, not the driver: a caller that
    /// retries after losing the answer must reuse the id, and a caller that wants a new
    /// epoch must wait for a proven stop (which this build does not offer yet, so a
    /// second operation is refused by the daemon).
    pub fn bind(
        root: Arc<ThreadRoot>,
        authorization: RootAuthorization,
        operation_id: String,
    ) -> Result<Self, ForegroundClientError> {
        let transport = SocketTransport { root };
        let binding = transport.bind(&authorization, &operation_id)?;
        let mut state = DriverState::new(transport, authorization, binding);
        // Adopt whatever the daemon already has committed for this epoch. Two cases land
        // here and both need it: a replay of a bind whose answer was lost (the earlier
        // handle is still reporting), and a host that restarted mid-turn. Starting from a
        // blank `idle` baseline would make this driver believe no turn is open and report
        // a sequence the daemon has already passed.
        // `refresh` adopts the daemon's view only when it is about this epoch, so a
        // failure here (no snapshot yet, or a refusal) simply leaves the baseline in
        // place. It is not an error to be the first driver of a root.
        let _ = state.refresh();
        Ok(Self { state })
    }

    /// The binding the daemon issued, including its narrow capability.
    pub fn binding(&self) -> &DriverBinding {
        &self.state.binding
    }

    /// Opens `turn_id`, which must be the only open turn.
    pub fn begin_turn(&mut self, turn_id: &str) -> Result<(), ForegroundClientError> {
        self.state.open_turn(turn_id)
    }

    /// Reports a waiting phase inside the currently open turn.
    pub fn waiting(&mut self, phase: ActivityPhase) -> Result<(), ForegroundClientError> {
        match phase {
            ActivityPhase::WaitingForChildren | ActivityPhase::WaitingForPermission => {}
            // `running`/`idle` are turn boundaries, not "still waiting", so they must go
            // through `begin_turn`/`finish_turn` where the ordering rules apply.
            _ => {
                return Err(ForegroundClientError::Protocol(format!(
                    "{phase:?} is not a waiting phase"
                )));
            }
        }
        let Some(turn) = self.state.committed.turn_id.clone() else {
            return Err(ForegroundClientError::Protocol(
                "no turn is open, so nothing can be waiting".into(),
            ));
        };
        self.state.report(Some(&turn), phase)
    }

    /// Closes `turn_id`, which must be the open turn.
    pub fn finish_turn(&mut self, turn_id: &str) -> Result<(), ForegroundClientError> {
        self.state.finish(turn_id)
    }

    /// The last snapshot the daemon confirmed, as this driver believes it.
    pub fn last_snapshot(&self) -> &ActivityUpdate {
        self.state.last_snapshot()
    }

    /// Whether a report is waiting for a retry that must reuse its sequence.
    pub fn has_pending_report(&self) -> bool {
        self.state.pending.is_some()
    }

    /// Replays an unconfirmed report verbatim. A no-op when nothing is pending.
    pub fn retry_pending(&mut self) -> Result<(), ForegroundClientError> {
        self.state.retry_pending()
    }

    /// Reads the daemon's committed snapshot, which is what a reconnected driver needs
    /// before deciding whether its pending report still describes the current turn.
    pub fn refresh(&mut self) -> Result<ActivitySnapshot, ForegroundClientError> {
        self.state.refresh()
    }
}

/// Recovery actions a host can run on its own lease.
///
/// A plain enum rather than four methods because the TUI's dispatch is synchronous
/// and can only hand one value across its control channel; the driver then runs
/// exactly one action under the lease it already holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoveryAction {
    /// Read what a freeze would stop. Never mutates.
    Inspect,
    /// Freeze and stop, then report whether a confirmation may be issued.
    Prepare { operation_id: String },
    /// Commit a prepared recovery.
    ///
    /// Carries no token. The confirmation token is issued to `prepare`, and the
    /// component that ran it keeps it in memory and hands it here through
    /// [`ConfirmedRecovery::token`]. Putting it in the command line would put a
    /// secret into shell history, `ps` output and the transcript.
    Confirm { operation_id: String },
    /// Read one operation's stored status.
    Operation { operation_id: String },
}

/// A confirmation plus the token that authorizes it.
///
/// Separate from [`RecoveryAction`] so the action itself stays printable and
/// loggable: `Debug` on this type redacts the token, and only the call site that
/// actually speaks to the daemon reads it. The token never crosses the control
/// channel as a command argument.
#[derive(Clone)]
pub struct ConfirmedRecovery {
    pub operation_id: String,
    token: String,
}

impl ConfirmedRecovery {
    pub fn new(operation_id: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            operation_id: operation_id.into(),
            token: token.into(),
        }
    }

    /// Only the daemon call reads this. Keeping it a method makes the read sites
    /// greppable, which is the point of `SecretCapability::expose`.
    pub fn token(&self) -> &str {
        &self.token
    }
}

impl std::fmt::Debug for ConfirmedRecovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfirmedRecovery")
            .field("operation_id", &self.operation_id)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// What one recovery action produced, shaped for the host to render.
///
/// `None` from the daemon's operation read survives as `status: None` rather than
/// becoming an error: "the runtime never ran this id" is an answer, and the caller
/// must be able to tell it apart from "the call failed".
#[derive(Clone, Debug)]
pub enum RecoveryOutcome {
    Impact(yi_agent_lifecycle::foreground::RecoveryImpact),
    Status(yi_agent_lifecycle::foreground::RecoveryOperationStatus),
    Operation(Option<yi_agent_lifecycle::foreground::RecoveryOperationStatus>),
}

/// One host process's authority to drive foreground roots.
///
/// This exists so the three hosts do not each re-derive two things that are easy to
/// get subtly wrong:
///
/// * **One lease per process, not per thread.** The control channel accounts for
///   *clients*; a host that registered a lease per conversation would look like N
///   frontends and would keep the runtime alive after the window closed.
/// * **Authorize once per root, reuse the authorization across turns.** The guardian
///   mints a new capability on every authorize, so re-authorizing each turn would churn
///   the authority table for no reason and would let a transient refusal silently
///   disable a conversation that was already working.
///
/// Every method is fallible only in the ways a caller can act on. `None` means "this
/// runtime does not serve the foreground contract for this root" and the host should
/// keep legacy semantics; it is never an error to surface to the user.
pub struct ForegroundHost {
    /// The lease this host acts with.
    ///
    /// Shared rather than owned: a frontend that already registered (the TUI, a
    /// managed CLI) must hand this host *that* lease. Registering a second one here
    /// would make one process look like two clients, and the guardian would then
    /// reclaim this host's work only when the other copy also left.
    lease: std::sync::Arc<yi_agent_lifecycle::client::FrontendLease>,
    /// Authorizations by root key. The queue is per process because a bind needs the
    /// same authorization across a retry around the host's own reconnection.
    authorized: std::sync::Mutex<std::collections::HashMap<String, RootAuthorization>>,
    /// Refusals by root key. A refusal is a decision, not a transient state: asking
    /// again on the next turn would let one "no" be answered "yes" by retrying.
    refused: std::sync::Mutex<std::collections::HashMap<String, String>>,
    /// Confirmation tokens issued to this host by its own `prepare`, by operation id.
    ///
    /// Kept here rather than handed back to the caller because a token that crossed
    /// the control channel would become a command argument, and a command argument
    /// lands in shell history and `ps`. The host is the only party that can run
    /// `confirm` on the same lease anyway, so it is the right holder.
    ///
    /// Not persisted: a token lives 60 seconds and dies with the process, which is
    /// the durability it is supposed to have.
    pending: std::sync::Mutex<std::collections::HashMap<String, ConfirmedRecovery>>,
}

impl ForegroundHost {
    /// Registers this process as a frontend of `runtime_dir`'s runtime.
    ///
    /// `Ok(None)` for embedded mode and for a runtime with no guardian: both are
    /// normal, and neither should stop a launch.
    pub fn connect(
        runtime_dir: &std::path::Path,
        kind: yi_agent_lifecycle::types::ClientKind,
        client_run_id: String,
    ) -> std::io::Result<Option<Self>> {
        let control_path = yi_agent_lifecycle::scope::control_path(runtime_dir);
        Self::connect_to(control_path, kind, client_run_id)
    }

    /// [`Self::connect`], against a guardian whose instance home the caller knows.
    ///
    /// The guardian derives its socket from the scope it was handed, so a caller that
    /// chose that scope must not re-derive it from the environment: on a machine with
    /// `YI_AGENT_HOME` set, the two would name different sockets.
    pub fn connect_in(
        runtime_dir: &std::path::Path,
        instance_home: &std::path::Path,
        kind: yi_agent_lifecycle::types::ClientKind,
        client_run_id: String,
    ) -> std::io::Result<Option<Self>> {
        Self::connect_to(
            yi_agent_lifecycle::scope::guardian_socket(runtime_dir, instance_home),
            kind,
            client_run_id,
        )
    }

    fn connect_to(
        control_path: std::path::PathBuf,
        kind: yi_agent_lifecycle::types::ClientKind,
        client_run_id: String,
    ) -> std::io::Result<Option<Self>> {
        let deadline =
            yi_agent_lifecycle::deadline::Deadline::after(std::time::Duration::from_secs(2));
        let Some(lease) = yi_agent_lifecycle::client::FrontendLease::register_if_served(
            &control_path,
            client_run_id,
            kind,
            yi_agent_lifecycle::types::LeaseMode::Business,
            &deadline,
        )?
        else {
            return Ok(None);
        };
        Ok(Some(Self::from_lease(std::sync::Arc::new(lease))))
    }

    /// A host that acts with a lease the caller already holds.
    ///
    /// This is the managed path's constructor. There is deliberately no "register if
    /// needed" variant: whether a lease exists is the caller's decision, and a host that
    /// silently made one would be the second frontend this type exists to avoid.
    pub fn from_lease(lease: std::sync::Arc<yi_agent_lifecycle::client::FrontendLease>) -> Self {
        Self {
            lease,
            authorized: std::sync::Mutex::new(std::collections::HashMap::new()),
            refused: std::sync::Mutex::new(std::collections::HashMap::new()),
            pending: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// This root's authorization, asking the guardian only the first time.
    ///
    /// Cached on purpose. The guardian mints a *new* capability on every authorize, so
    /// asking per turn would churn the authority table and -- worse -- let one transient
    /// refusal disable a conversation that was already reporting. `None` means the
    /// guardian said no (or is not there), which the host treats as legacy semantics.
    pub fn authorization(
        &self,
        root: &Arc<crate::thread_root::ThreadRoot>,
        operation_id: &str,
    ) -> Result<Option<RootAuthorization>, String> {
        Ok(self.authority(root, operation_id)?.into_authorization())
    }

    /// The same question with the *reason* kept.
    ///
    /// Delegation needs it: "no guardian" is embedded mode and may fall back to this
    /// host's own secret, while "the guardian refused" must not. Flattening both to
    /// `None` is precisely how a policy refusal turned into permission.
    pub fn authority(
        &self,
        root: &Arc<crate::thread_root::ThreadRoot>,
        operation_id: &str,
    ) -> Result<crate::thread_root::ForegroundAuthority, String> {
        let key = crate::thread_root::application_root_key(root.thread_id());
        if let Some(existing) = self
            .refused
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&key)
            .cloned()
        {
            return Ok(crate::thread_root::ForegroundAuthority::Refused(existing));
        }
        let mut authorized = self
            .authorized
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(existing) = authorized.get(&key) {
            return Ok(crate::thread_root::ForegroundAuthority::Managed(
                existing.clone(),
            ));
        }
        // A managed root already holds the authority the shell brokered for it. Asking
        // the guardian a second time would mint another capability for the same root
        // and -- on a frontend that registered once -- look like a second client.
        if root.is_managed() {
            let Some(fresh) = root.managed_root_authorization() else {
                // Nothing brokered yet: the attach that would have installed the entry
                // has not happened, so there is nothing to bind with. Legacy semantics
                // (embedded), deliberately not `Refused` -- nobody said no.
                return Ok(crate::thread_root::ForegroundAuthority::Embedded);
            };
            authorized.insert(key, fresh.clone());
            return Ok(crate::thread_root::ForegroundAuthority::Managed(fresh));
        }
        match root.foreground_authority(&self.lease, operation_id)? {
            crate::thread_root::ForegroundAuthority::Managed(fresh) => {
                authorized.insert(key, fresh.clone());
                Ok(crate::thread_root::ForegroundAuthority::Managed(fresh))
            }
            crate::thread_root::ForegroundAuthority::Embedded => {
                Ok(crate::thread_root::ForegroundAuthority::Embedded)
            }
            crate::thread_root::ForegroundAuthority::Refused(reason) => {
                self.refused
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(key, reason.clone());
                Ok(crate::thread_root::ForegroundAuthority::Refused(reason))
            }
        }
    }

    /// Runs one recovery action on this host's own lease.
    ///
    /// Deliberately a method on the host rather than a separate type holding the
    /// lease: `FrontendLease` is not `Clone` (a copy would renew and release
    /// independently, and release is destructive), so the only way to run recovery on
    /// the same credential the driver uses is to borrow it here.
    ///
    /// `fence` names the attempt the caller believes is current. It always comes from
    /// what this host already learned (a binding the daemon issued), never from a
    /// caller-supplied id: a fence built out of an id would describe a conversation
    /// nobody verified.
    pub fn recover(
        &self,
        fence: &yi_agent_lifecycle::foreground::ForegroundFence,
        action: &RecoveryAction,
    ) -> Result<RecoveryOutcome, ForegroundClientError> {
        let deadline =
            yi_agent_lifecycle::deadline::Deadline::after(std::time::Duration::from_secs(10));
        match action {
            RecoveryAction::Inspect => self
                .lease
                .inspect_root_recovery(fence, &deadline)
                .map(RecoveryOutcome::Impact)
                .map_err(|error| ForegroundRecoveryHost::classify("inspect", error)),
            RecoveryAction::Prepare { operation_id } => {
                let request = yi_agent_lifecycle::foreground::RecoveryRequest {
                    expected: fence.clone(),
                    operation_id: operation_id.clone(),
                };
                match self.lease.prepare_root_recovery(&request, &deadline) {
                    Ok(Some(status)) => {
                        // Remember the token only when the daemon actually issued one.
                        // A blocked preparation has no token, and storing a placeholder
                        // would make `confirm` look available for work that is still
                        // running.
                        if let yi_agent_lifecycle::foreground::RecoveryOperationStatus::Ready(
                            preview,
                        ) = &status
                            && let Some(token) = &preview.confirmation_token
                        {
                            self.remember_confirmation(ConfirmedRecovery::new(
                                operation_id.clone(),
                                token.expose(),
                            ));
                        }
                        Ok(RecoveryOutcome::Status(status))
                    }
                    Ok(None) => Err(ForegroundClientError::Protocol(
                        "the daemon has no record of this recovery operation".into(),
                    )),
                    Err(error) => Err(ForegroundRecoveryHost::classify("prepare", error)),
                }
            }
            RecoveryAction::Confirm { operation_id } => {
                let confirmed = self
                    .pending_confirmation(operation_id)
                    .ok_or_else(|| {
                        ForegroundClientError::Rejected(
                            yi_agent_lifecycle::foreground::ForegroundFailure::new(
                                yi_agent_lifecycle::foreground::ForegroundErrorCode::ConfirmationRequired,
                                "confirm: no prepared token is held for this operation; run prepare first",
                            ),
                        )
                    })?;
                let request = yi_agent_lifecycle::foreground::RecoveryRequest {
                    expected: fence.clone(),
                    operation_id: operation_id.clone(),
                };
                let secret = yi_agent_lifecycle::foreground::SecretCapability::new(
                    confirmed.token().to_string(),
                );
                match self
                    .lease
                    .confirm_root_recovery(&request, &secret, &deadline)
                {
                    Ok(Some(status)) => Ok(RecoveryOutcome::Status(status)),
                    Ok(None) => Err(ForegroundClientError::Protocol(
                        "the daemon has no record of this recovery operation".into(),
                    )),
                    Err(error) => Err(ForegroundRecoveryHost::classify("confirm", error)),
                }
            }
            RecoveryAction::Operation { operation_id } => self
                .lease
                .root_recovery_operation(&fence.root, operation_id, &deadline)
                .map(RecoveryOutcome::Operation)
                .map_err(|error| ForegroundRecoveryHost::classify("operation", error)),
        }
    }

    /// Keeps the token a successful prepare issued, so the matching confirm can use it.
    ///
    /// Replacing an older token for the same id is correct: a re-prepare mints a new
    /// token and invalidates the old one, so keeping both would let a stale value be
    /// tried against a live operation.
    fn remember_confirmation(&self, confirmed: ConfirmedRecovery) {
        self.pending
            .lock()
            .expect("pending confirmation mutex poisoned")
            .insert(confirmed.operation_id.clone(), confirmed);
    }

    /// The token this host holds for `operation_id`, if any.
    fn pending_confirmation(&self, operation_id: &str) -> Option<ConfirmedRecovery> {
        self.pending
            .lock()
            .expect("pending confirmation mutex poisoned")
            .get(operation_id)
            .cloned()
    }

    /// Opens a turn for `root`, binding this host as its driver on first use.
    ///
    /// Returns `Ok(None)` whenever the contract is not offered (no guardian, a
    /// refusal, an unsupported build) -- the host then runs the turn with legacy
    /// semantics. A [`ForegroundTurn`] is returned only when the daemon really issued a
    /// binding, because reporting activity against an unbound root would be a lie.
    pub fn begin(
        &self,
        root: &Arc<crate::thread_root::ThreadRoot>,
        operation_id: &str,
        turn_id: &str,
    ) -> Result<Option<ForegroundTurn>, String> {
        let Some(authorization) = self.authorization(root, operation_id)? else {
            return Ok(None);
        };
        let mut turn = match root.bind_foreground_driver(authorization, operation_id)? {
            Some(turn) => turn,
            None => return Ok(None),
        };
        turn.begin(turn_id);
        Ok(Some(turn))
    }
}

/// The host's recovery entry point: inspect, prepare, confirm, query.
///
/// Deliberately *not* reachable from a thread id. Recovery stops a running
/// conversation, so the ability to do it has to come from something the caller
/// already earned -- the host's own lease. `new` therefore takes that lease, and
/// there is no constructor that resolves one from an id. Nothing here is a model
/// tool: a recovery an LLM could trigger by asserting `confirmed=true` would make
/// the preview step theatre.
///
/// The lease is taken by value rather than borrowed from a binding because the
/// binding does not own one: the guardian issues a lease to a *frontend*, and the
/// host is the frontend. Passing the lease in is what keeps recovery on the same
/// credential the driver uses instead of minting a second client.
///
/// Every method keeps the design's shape. `inspect` reads stored state and never
/// freezes; `prepare` is the first mutating step and may come back `Blocked` with
/// the impact it could not prove stopped; `confirm` is the only way to commit. A
/// lost answer is resolved by querying the *same* operation id, never by
/// preparing a new one.
pub struct ForegroundRecoveryHost {
    lease: yi_agent_lifecycle::client::FrontendLease,
}

impl ForegroundRecoveryHost {
    /// Builds the entry point from the host's own lease.
    pub fn new(lease: yi_agent_lifecycle::client::FrontendLease) -> Self {
        Self { lease }
    }

    fn deadline() -> yi_agent_lifecycle::deadline::Deadline {
        yi_agent_lifecycle::deadline::Deadline::after(std::time::Duration::from_secs(5))
    }

    /// Reads what a recovery of this root would stop. Never freezes, never mutates.
    ///
    /// This is the honest meaning of "inspect": the daemon's own read-only entry
    /// reports the impact without writing a freeze row, so a host can show the user
    /// what is at stake while the conversation keeps running. A caller that used
    /// `prepare` for this would stop work the user had not yet agreed to stop.
    pub fn inspect(
        &mut self,
        expected: &yi_agent_lifecycle::foreground::ForegroundFence,
    ) -> Result<yi_agent_lifecycle::foreground::RecoveryImpact, ForegroundClientError> {
        self.lease
            .inspect_root_recovery(expected, &Self::deadline())
            .map_err(|error| Self::classify("inspect", error))
    }

    /// The first mutating step: freeze, stop, and issue a token only if proven.
    pub fn prepare(
        &mut self,
        request: &yi_agent_lifecycle::foreground::RecoveryRequest,
    ) -> Result<yi_agent_lifecycle::foreground::RecoveryOperationStatus, ForegroundClientError>
    {
        match self.lease.prepare_root_recovery(request, &Self::deadline()) {
            Ok(Some(status)) => Ok(status),
            Ok(None) => Err(ForegroundClientError::Protocol(
                "the daemon has no record of this recovery operation".into(),
            )),
            Err(error) => Err(Self::classify("prepare", error)),
        }
    }

    /// Commits a prepared recovery with the token `prepare` returned.
    pub fn confirm(
        &mut self,
        request: &yi_agent_lifecycle::foreground::RecoveryRequest,
        token: &SecretCapability,
    ) -> Result<yi_agent_lifecycle::foreground::RecoveryOperationStatus, ForegroundClientError>
    {
        match self
            .lease
            .confirm_root_recovery(request, token, &Self::deadline())
        {
            Ok(Some(status)) => Ok(status),
            Ok(None) => Err(ForegroundClientError::Protocol(
                "the daemon has no record of this recovery operation".into(),
            )),
            Err(error) => Err(Self::classify("confirm", error)),
        }
    }

    /// Reads one operation's stored status.
    ///
    /// This is how a lost answer is resolved: the caller asks about the *same*
    /// operation instead of minting a new one, and a `NeedsPreview` answer means a
    /// restart dropped the token rather than that the work was undone.
    pub fn operation(
        &mut self,
        root: &RootRef,
        id: &str,
    ) -> Result<
        Option<yi_agent_lifecycle::foreground::RecoveryOperationStatus>,
        ForegroundClientError,
    > {
        self.lease
            .root_recovery_operation(root, id, &Self::deadline())
            .map_err(|error| Self::classify("operation", error))
    }

    /// Maps a control-channel error onto the caller-visible split.
    ///
    /// `Unsupported` is a refusal, not a transport failure: the build simply does
    /// not serve recovery, and a host that retried it would spin. The rest keeps the
    /// design's distinction between "refused" (act on it) and "unknown" (query the
    /// same operation).
    pub(crate) fn classify(stage: &'static str, error: std::io::Error) -> ForegroundClientError {
        match error.kind() {
            std::io::ErrorKind::Unsupported => ForegroundClientError::Rejected(
                yi_agent_lifecycle::foreground::ForegroundFailure::new(
                    ForegroundErrorCode::Unsupported,
                    format!("{stage}: this runtime does not serve recovery"),
                ),
            ),
            std::io::ErrorKind::InvalidInput
            | std::io::ErrorKind::PermissionDenied
            | std::io::ErrorKind::NotFound => ForegroundClientError::Rejected(
                yi_agent_lifecycle::foreground::ForegroundFailure::new(
                    ForegroundErrorCode::AuthorityDenied,
                    format!("{stage}: the recovery was refused"),
                ),
            ),
            _ => ForegroundClientError::Transport(format!("{stage}: {error}")),
        }
    }
}

/// Runs one activity write and reports its failure as a diagnostic, never as an error.
///
/// The hosts call this around `ForegroundTurn` so that the rule "activity is
/// description, never control" lives in one place instead of being re-derived by
/// three entry points. A host that used `?` here would abort a working conversation
/// because a display update failed, which is exactly backwards.
///
/// Returns the warning the caller may surface, or `None` when the write succeeded.
/// No secret reaches the message: `ForegroundClientError` is built to print without
/// its capability, and the stage names are literals.
pub fn foreground_activity_sync_warning(
    outcome: Result<(), ForegroundClientError>,
    stage: &'static str,
) -> Option<String> {
    match outcome {
        Ok(()) => None,
        Err(error) => {
            tracing::warn!(stage, %error, "foreground activity could not be reported");
            Some(format!("{stage}: {error}"))
        }
    }
}

/// A host's per-turn view of one foreground root.
///
/// The three hosts (desktop, TUI, headless) all need the same sequence: open the turn
/// before any work, close it when the turn really ends, and never let a failed report
/// turn a working conversation into a failed one. Putting that here means the hosts
/// cannot disagree about it, and Kanban still gets no special branch.
///
/// The invariant is that activity is *description*, never control: every method
/// returns `()` and reports problems through [`Self::warning`] (and a `tracing::warn`
/// line that carries no secrets). A caller that wanted to abort a turn because the
/// activity write failed would be turning a display problem into a lost answer.
///
/// `allow(dead_code)`: the hosts are wired in a follow-up (app-server's turn driver,
/// the TUI loop and headless). Until then nothing constructs this type, and deleting
/// it would throw away the tested ordering machine that the wiring needs. The
/// attribute goes away with the last host, not before.
#[allow(dead_code)]
pub struct ForegroundTurn<T: ForegroundTransport = SocketTransport> {
    driver: DriverState<T>,
    /// The turn this handle opened.
    ///
    /// Kept so `finish` can name it without the host threading the id through every
    /// exit path. A host that had to remember it would eventually pass the wrong one
    /// after a resume, and the daemon would refuse the close.
    turn_id: String,
    /// The last problem this turn saw, for the host's diagnostics panel.
    warning: Option<String>,
}

#[allow(dead_code)]
impl ForegroundTurn<SocketTransport> {
    /// Binds the host as this root's driver.
    ///
    /// A refusal here is a normal outcome, not an error to propagate: a build or a
    /// runtime that does not serve the foreground contract must still run the
    /// conversation, just without activity reporting (the legacy policy).
    pub fn bind(
        root: Arc<ThreadRoot>,
        authorization: RootAuthorization,
        operation_id: String,
    ) -> Result<Self, ForegroundClientError> {
        let driver = ForegroundDriver::bind(root, authorization, operation_id)?;
        Ok(Self {
            driver: driver.state,
            turn_id: String::new(),
            warning: None,
        })
    }
}

#[allow(dead_code)]
impl<T: ForegroundTransport> ForegroundTurn<T> {
    /// Opens `turn_id`. A failure is recorded and swallowed.
    ///
    /// Opening the turn that is already open is a no-op, not a failure: hosts retry
    /// around their own recovery, and a retry is not a second statement.
    pub fn begin(&mut self, turn_id: &str) {
        if self.driver.committed.turn_id.as_deref() == Some(turn_id) {
            self.turn_id = turn_id.to_string();
            self.warning = None;
            return;
        }
        if let Err(error) = self.driver.open_turn(turn_id) {
            self.record("begin", error);
            return;
        }
        self.turn_id = turn_id.to_string();
        self.warning = None;
    }

    /// Reports a waiting phase inside the open turn. Same posture as `begin`.
    pub fn waiting(&mut self, phase: ActivityPhase) {
        let Some(turn) = self.driver.committed.turn_id.clone() else {
            self.record(
                "waiting",
                ForegroundClientError::Protocol(
                    "no turn is open, so nothing can be waiting".into(),
                ),
            );
            return;
        };
        if let Err(error) = self.driver.report(Some(&turn), phase) {
            self.record("waiting", error);
        }
    }

    /// Closes `turn_id`. Must run on every exit path -- success, provider error and
    /// cancellation -- because a turn left open would make the next one look like a
    /// second statement about a turn that never ended.
    /// Closes the turn [`Self::begin`] opened.
    ///
    /// Takes no id: the handle already knows which turn it opened, and a host forced to
    /// thread the id through every exit path would eventually pass a stale one after a
    /// resume -- which the daemon would refuse, leaving the turn open for the next one
    /// to collide with.
    pub fn finish(&mut self) {
        // Already closed: the overlapping exit paths both reached the same end, and
        // reporting that as a problem would train the host to ignore real ones.
        if self.driver.committed.turn_id.is_none() {
            return;
        }
        let turn_id = self.turn_id.clone();
        if let Err(error) = self.driver.finish(&turn_id) {
            self.record("finish", error);
        } else {
            self.warning = None;
        }
    }

    /// The last problem, if any. The host shows this; it never retries a turn because
    /// of it.
    pub fn warning(&self) -> Option<&str> {
        self.warning.as_deref()
    }

    /// The binding the daemon issued, for hosts that need to name the fence.
    pub fn binding(&self) -> &DriverBinding {
        &self.driver.binding
    }

    fn record(&mut self, stage: &'static str, error: ForegroundClientError) {
        // Diagnostics carry the stage and the error's own text, never the capability:
        // `ForegroundClientError` does not print secrets by construction.
        tracing::warn!(stage, %error, "foreground activity could not be reported");
        self.warning = Some(format!("{stage}: {error}"));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use yi_agent_lifecycle::foreground::ForegroundFence;

    use super::*;

    /// What the fake daemon did with each report, in order.
    #[derive(Default)]
    struct FakeTransport {
        sent: Mutex<Vec<ActivityUpdate>>,
        /// How many reports fail with a transport error before succeeding.
        fail_next: Mutex<usize>,
        /// Reject the next report with this code, if set.
        reject_next: Mutex<Option<ForegroundErrorCode>>,
    }

    impl FakeTransport {
        fn sent(&self) -> Vec<ActivityUpdate> {
            self.sent.lock().unwrap().clone()
        }
    }

    impl ForegroundTransport for FakeTransport {
        fn read_activity(
            &self,
            _authorization: &RootAuthorization,
            _root: &RootRef,
        ) -> Result<IpcResponse, ForegroundClientError> {
            // The state-machine tests never read back; they assert on what was sent.
            // A daemon-backed read is covered by `tests/foreground_driver.rs`.
            Err(ForegroundClientError::Protocol(
                "the fake transport does not serve reads".into(),
            ))
        }

        fn bind(
            &self,
            _authorization: &RootAuthorization,
            _operation_id: &str,
        ) -> Result<DriverBinding, ForegroundClientError> {
            unreachable!("the state machine is constructed with a binding")
        }

        fn set_activity(
            &self,
            _capability: &SecretCapability,
            update: &ActivityUpdate,
        ) -> Result<ActivitySnapshot, ForegroundClientError> {
            let mut fail_next = self.fail_next.lock().unwrap();
            if *fail_next > 0 {
                *fail_next -= 1;
                return Err(ForegroundClientError::Transport("lost the answer".into()));
            }
            if let Some(code) = self.reject_next.lock().unwrap().take() {
                return Err(ForegroundClientError::Rejected(ForegroundFailure::new(
                    code, "refused",
                )));
            }
            self.sent.lock().unwrap().push(update.clone());
            Ok(ActivitySnapshot {
                update: update.clone(),
                changed_at: "2026-11-04T00:00:00Z".into(),
            })
        }
    }

    fn binding(last_seq: u64) -> DriverBinding {
        DriverBinding {
            fence: ForegroundFence {
                root: RootRef {
                    session_id: "session-1".into(),
                    root_id: "root-1".into(),
                    attempt_id: "attempt-1".into(),
                },
                generation: "gen-1".into(),
                binding_epoch: 1,
                driver_epoch: 2,
            },
            last_seq,
            driver_capability: SecretCapability::new("driver-cap"),
        }
    }

    fn driver(fake: FakeTransport, last_seq: u64) -> DriverState<FakeTransport> {
        DriverState::new(
            fake,
            RootAuthorization {
                scope_key: "scope".into(),
                daemon_instance_id: "gen-1".into(),
                client_run_id: "owner".into(),
                root_id: "root-1".into(),
                capability: "cap".into(),
            },
            binding(last_seq),
        )
    }

    #[test]
    fn foreground_root_lifecycle_a_turn_opens_finishes_and_the_next_one_continues() {
        // The plan's own example: the sequence keeps rising across turns, and a finish
        // for the *old* turn is refused even after a newer one opened.
        let mut driver = driver(FakeTransport::default(), 0);
        let first = driver.last_snapshot().clone();

        driver.open_turn("turn-1").unwrap();
        driver.finish("turn-1").unwrap();
        driver.open_turn("turn-2").unwrap();

        assert_eq!(driver.last_snapshot().turn_id.as_deref(), Some("turn-2"));
        assert!(driver.last_snapshot().seq > first.seq);
        assert!(
            driver.finish("turn-1").is_err(),
            "a late finish of the closed turn must not close turn-2"
        );
    }

    #[test]
    fn foreground_root_lifecycle_the_first_report_starts_after_the_bound_sequence() {
        // A driver that restarted must not rewind to 1: the daemon committed 7, so the
        // first new statement is 8.
        let mut driver = driver(FakeTransport::default(), 7);
        driver.open_turn("turn-9").unwrap();
        assert_eq!(driver.last_snapshot().seq, 8);
        assert_eq!(driver.last_snapshot().phase, ActivityPhase::Running);
    }

    #[test]
    fn foreground_root_lifecycle_a_second_turn_cannot_open_while_one_is_live() {
        let mut driver = driver(FakeTransport::default(), 0);
        driver.open_turn("turn-1").unwrap();
        let error = driver.open_turn("turn-2").unwrap_err();
        assert!(matches!(error, ForegroundClientError::Protocol(_)));
        assert_eq!(driver.last_snapshot().turn_id.as_deref(), Some("turn-1"));
    }

    #[test]
    fn foreground_root_lifecycle_a_transport_failure_is_replayed_with_the_same_sequence() {
        // The daemon may have applied the report before the answer was lost. Retrying
        // with a *new* sequence would look like a new statement; retrying the same pair
        // reduces to `Duplicate` on the daemon.
        let fake = FakeTransport::default();
        *fake.fail_next.lock().unwrap() = 1;
        let mut driver = driver(fake, 3);
        let error = driver.open_turn("turn-1").unwrap_err();
        assert!(matches!(error, ForegroundClientError::Transport(_)));
        assert!(driver.pending.is_some(), "the report must be kept");
        assert_eq!(
            driver.last_snapshot().seq,
            3,
            "a failed report does not advance the committed sequence"
        );

        driver.retry_pending().unwrap();
        assert!(driver.pending.is_none());
        let sent = driver.transport.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].seq, 4, "the replay keeps the original sequence");
    }

    #[test]
    fn foreground_root_lifecycle_a_new_report_is_refused_while_one_is_unconfirmed() {
        let fake = FakeTransport::default();
        *fake.fail_next.lock().unwrap() = 1;
        let mut driver = driver(fake, 0);
        driver.open_turn("turn-1").unwrap_err();
        let error = driver
            .report(Some("turn-1"), ActivityPhase::WaitingForChildren)
            .unwrap_err();
        assert!(matches!(error, ForegroundClientError::Protocol(_)));
        assert_eq!(driver.transport.sent().len(), 0);
    }

    #[test]
    fn foreground_root_lifecycle_a_refusal_does_not_leave_a_pending_report() {
        let fake = FakeTransport::default();
        *fake.reject_next.lock().unwrap() = Some(ForegroundErrorCode::StaleDriver);
        let mut driver = driver(fake, 0);
        let error = driver.open_turn("turn-1").unwrap_err();
        assert!(
            matches!(
                error,
                ForegroundClientError::Rejected(ForegroundFailure {
                    code: ForegroundErrorCode::StaleDriver,
                    ..
                })
            ),
            "a refusal must be reported as a refusal, not as an ambiguous failure"
        );
        assert!(driver.pending.is_none());
        assert_eq!(driver.last_snapshot().seq, 0);
    }

    #[test]
    fn foreground_root_lifecycle_a_driver_cannot_report_unknown() {
        let fake = FakeTransport::default();
        let mut driver = driver(fake, 0);
        driver.open_turn("turn-1").unwrap();
        let error = driver
            .report(Some("turn-1"), ActivityPhase::Unknown)
            .unwrap_err();
        assert!(matches!(error, ForegroundClientError::Protocol(_)));
        assert_eq!(
            driver.transport.sent().len(),
            1,
            "only the open turn was sent"
        );
    }

    /// A host helper over a driver whose transport is a fake.
    ///
    /// `ForegroundTurn` is generic over nothing, so the test builds the real struct
    /// from a real `ForegroundDriver` and swaps the transport for the fake -- the
    /// failure posture is what is under test, not the transport.
    fn turn_over(fake: FakeTransport, last_seq: u64) -> ForegroundTurn<FakeTransport> {
        ForegroundTurn {
            driver: driver(fake, last_seq),
            turn_id: String::new(),
            warning: None,
        }
    }

    #[test]
    fn foreground_root_lifecycle_a_host_turn_swallows_a_failed_report() {
        // The whole point of the host wrapper: a lost activity report must leave the
        // conversation running and just be visible as a warning.
        let fake = FakeTransport::default();
        *fake.fail_next.lock().unwrap() = 1;
        let mut turn = turn_over(fake, 0);

        turn.begin("turn-1");
        assert!(
            turn.warning().is_some(),
            "a failed begin is recorded, not returned"
        );
        assert_eq!(
            turn.driver.last_snapshot().seq,
            0,
            "the committed sequence did not advance"
        );
    }

    #[test]
    fn foreground_root_lifecycle_a_host_turn_clears_its_warning_on_success() {
        let fake = FakeTransport::default();
        *fake.fail_next.lock().unwrap() = 1;
        let mut turn = turn_over(fake, 0);
        turn.begin("turn-1");
        assert!(turn.warning().is_some());

        // The retry is the host's business, not the wrapper's; once the turn opens the
        // stale warning must not keep describing a problem that is over.
        turn.driver.retry_pending().unwrap();
        turn.begin("turn-1");
        assert!(turn.warning().is_none());
    }

    #[test]
    fn foreground_root_lifecycle_a_host_turn_finish_is_idempotent_from_the_host_side() {
        // Exit paths overlap (error plus cancellation both finalize). A second finish
        // must not panic and must not surface as a warning the user has to act on.
        let fake = FakeTransport::default();
        let mut turn = turn_over(fake, 0);
        turn.begin("turn-1");
        turn.finish();
        turn.finish();
        assert!(
            turn.warning().is_none(),
            "a duplicate finish is a no-op, not a problem to report"
        );
    }

    /// Recovery is reachable only from a host that already holds an authorization.
    ///
    /// The point of the type is that there is no path from "I know a thread id" to
    /// "I can stop that conversation". A constructor that took an id and looked the
    /// rest up would be exactly that path, so the only way in is an authorization
    /// the host already had to earn through its own lease.
    #[test]
    fn foreground_root_lifecycle_recovery_host_needs_a_host_authorization() {
        // The type has no `from_thread_id`: the only way in is a lease the host
        // already holds, so knowing a thread id does not grant recovery.
        let _constructor: fn(yi_agent_lifecycle::client::FrontendLease) -> ForegroundRecoveryHost =
            ForegroundRecoveryHost::new;
    }

    /// Recovery is not a model tool.
    ///
    /// A recovery that an LLM could trigger with `confirmed=true` would defeat the
    /// whole preview/confirm design, so the host type lives outside any tool
    /// registry and nothing here implements a tool interface.
    #[test]
    fn foreground_root_lifecycle_recovery_is_not_a_model_tool() {
        // The durable guarantee is structural: this type needs a lease the host
        // earned, and a model has no way to hold one. What this scan adds is a cheap
        // tripwire for the one way a future edit could undo that -- giving the type a
        // tool adapter. The marker is assembled so the scan cannot match this test.
        let source = include_str!("foreground.rs");
        let tool_impl = ["impl ", "Tool", " for ForegroundRecoveryHost"].concat();
        assert!(
            !source.contains(&tool_impl),
            "recovery must not be reachable as a model tool"
        );
    }

    /// The token the host keeps after a prepare must never appear in a `Debug`
    /// rendering. A confirmation is logged by callers that hold the struct, and a
    /// secret in a log line is a secret in whatever the user later shares.
    #[test]
    fn foreground_root_lifecycle_confirmed_recovery_redacts_its_token() {
        let confirmed = ConfirmedRecovery::new("op-1", "super-secret-token");
        let rendered = format!("{confirmed:?}");
        assert!(rendered.contains("op-1"));
        assert!(
            !rendered.contains("super-secret-token"),
            "the debug rendering leaked the token: {rendered}"
        );
        assert!(rendered.contains("redacted"));
        // The token is still reachable where it is actually needed.
        assert_eq!(confirmed.token(), "super-secret-token");
    }

    /// Only a prepare that the daemon actually issued a token for may be remembered.
    ///
    /// A blocked preparation carries no token, and storing a placeholder would make
    /// `confirm` look available for work that is still running. The rule is asserted
    /// over the status shape rather than a live daemon, because the point is what the
    /// host does with an existing answer.
    #[test]
    fn foreground_root_lifecycle_only_a_ready_preview_yields_a_token() {
        use yi_agent_lifecycle::foreground::{
            RecoveryImpact, RecoveryOperationStatus, RecoveryPreview, RecoveryRequest, RootRef,
            SecretCapability,
        };

        let root = RootRef {
            session_id: "s".into(),
            root_id: "r".into(),
            attempt_id: "a".into(),
        };
        let request = RecoveryRequest {
            expected: ForegroundFence {
                root: root.clone(),
                generation: "g".into(),
                binding_epoch: 1,
                driver_epoch: 1,
            },
            operation_id: "op-1".into(),
        };
        let impact = RecoveryImpact {
            root: root.clone(),
            stopped_execution_ids: Vec::new(),
            blocked_execution_ids: vec!["exec-1".into()],
            child_ids: Vec::new(),
            version: 1,
            reason: "frozen".into(),
        };

        // Blocked: no token to remember.
        let blocked = RecoveryOperationStatus::Blocked(impact.clone());
        assert!(matches!(blocked, RecoveryOperationStatus::Blocked(_)));

        // Ready: exactly the shape whose token the host stores.
        let ready = RecoveryOperationStatus::Ready(RecoveryPreview {
            request,
            impact,
            confirmation_token: Some(SecretCapability::new("token-1")),
            expires_at: Some("2026-11-04T00:00:00Z".into()),
        });
        match ready {
            RecoveryOperationStatus::Ready(preview) => {
                assert_eq!(
                    preview
                        .confirmation_token
                        .expect("a ready preview must carry a token")
                        .expose(),
                    "token-1"
                );
            }
            other => panic!("expected ready, got {other:?}"),
        }
    }

    #[test]
    fn foreground_root_lifecycle_waiting_requires_an_open_turn() {
        let fake = FakeTransport::default();
        let mut driver = driver(fake, 0);
        assert!(
            driver
                .report(Some("turn-1"), ActivityPhase::WaitingForChildren)
                .is_err(),
            "waiting outside a turn is not a legal transition"
        );
    }
}
