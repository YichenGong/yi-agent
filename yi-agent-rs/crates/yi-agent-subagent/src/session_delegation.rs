//! The host side of session delegation: one binding per conversation, one turn
//! client per run.
//!
//! The whole point is that a **conversation** is not an execution. Its last run
//! may have ended in `stalled` and that must not make `spawn_agent` impossible
//! forever; what gates a write is the live binding and the turn that is open
//! right now. This module owns both, and splits them so a tool can never hold the
//! wider one:
//!
//! * [`SessionHost`] holds the binding capability. It never leaves the host, and
//!   no tool ever receives it.
//! * [`SessionTurnClient`] holds the turn secret plus a fixed socket. It is a
//!   snapshot of *this* run; a later turn cannot be borrowed through it, because
//!   nothing in it can be refreshed.
//!
//! Failure is reported as [`SessionClientError`] and, in particular, a transport
//! failure is never retried for a write that changes daemon state: a lost reply
//! means the spawn may already have happened.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use yi_agent_lifecycle::foreground::ExecutionIdentity;
use yi_agent_lifecycle::session_delegation::{
    PrepareSession, SessionBinding, SessionFailure, SessionFailureCode, SessionFence,
    SessionIdentity, SessionRef, SessionTurn,
};
use yi_agent_store::ipc::{
    IpcRequest, IpcResponse, PROTOCOL_VERSION, RequestEnvelope, ResponseEnvelope,
    SessionHostControlFrame, SessionHostControlRequest, send_request,
};

use crate::binding::{RuntimeHandle, is_liveness_error};
use crate::thread_root::{SessionContainerAttachment, ThreadRoot};

/// How a host-side session call failed.
///
/// Three kinds, kept apart because they mean different things to a model: the
/// daemon made a decision, the transport broke, or the two sides disagree about
/// the protocol. Only the first is about this conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionClientError {
    /// The daemon answered with a stable `SessionFailure`.
    Rejected(SessionFailure),
    /// The request did not complete. For a write, the outcome is unknown.
    Transport(String),
    /// The daemon does not speak this lifecycle, or answered something else.
    Protocol(String),
}

impl SessionClientError {
    pub fn code(&self) -> Option<SessionFailureCode> {
        match self {
            Self::Rejected(failure) => Some(failure.code),
            _ => None,
        }
    }

    /// Whether the caller may retry the *same* request safely.
    ///
    /// Only a rejection is a decision: a stale binding or a closed turn can be
    /// fixed and retried. A transport failure on a write is explicitly not
    /// retryable here -- the request may have been applied, and "retry" would then
    /// be a second real side effect.
    pub fn is_retryable_decision(&self) -> bool {
        matches!(
            self,
            Self::Rejected(SessionFailure {
                code: SessionFailureCode::StaleBinding
                    | SessionFailureCode::StaleGeneration
                    | SessionFailureCode::StaleTurn
                    | SessionFailureCode::AdmissionClosed,
                ..
            })
        )
    }
}

impl std::fmt::Display for SessionClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(failure) => {
                write!(
                    formatter,
                    "session delegation refused ({:?}): {}",
                    failure.code, failure.reason
                )
            }
            Self::Transport(message) => {
                write!(formatter, "session delegation transport: {message}")
            }
            Self::Protocol(message) => write!(formatter, "session delegation protocol: {message}"),
        }
    }
}

impl std::error::Error for SessionClientError {}

fn failure_from_response(response: IpcResponse) -> SessionClientError {
    match response {
        IpcResponse::SessionDelegationRejected { failure } => SessionClientError::Rejected(failure),
        IpcResponse::UnsupportedProtocol { .. } => SessionClientError::Protocol(
            "the daemon does not advertise the session delegation lifecycle".to_string(),
        ),
        IpcResponse::Error { code, message } => SessionClientError::Protocol(format!(
            "{code}: {}",
            message.unwrap_or_else(|| code.to_string())
        )),
        other => SessionClientError::Protocol(format!("unexpected session response: {other:?}")),
    }
}

/// One conversation's host: the binding, reused for the life of the driver.
///
/// Prepared once and kept, not rebuilt per turn: a new binding per turn would look
/// like a driver replacement every time and demand a stop proof that has nothing
/// to prove.
pub struct SessionHost {
    root: Arc<ThreadRoot>,
    identity: SessionIdentity,
    /// A replacement resume digest, written only when this host upgrades an
    /// existing capability. `None` for a normal re-prepare.
    replacement_resume_digest: Option<String>,
    host_run_id: String,
    attached: SessionContainerAttachment,
    live: Mutex<Option<SessionBinding>>,
    /// The last turn this host opened and could not prove closed.
    ///
    /// A host that begins a new turn while this is set would be running two turns
    /// for one conversation, so it refuses instead.
    unclosed: Mutex<Option<String>>,
}

impl SessionHost {
    /// Builds a host for one already-attached container.
    ///
    /// `host_run_id` names this process as the owner; it is not authority. A new
    /// process that only repeats it still cannot get the binding back.
    pub fn new(
        root: Arc<ThreadRoot>,
        identity: SessionIdentity,
        replacement_resume_digest: Option<String>,
        attached: SessionContainerAttachment,
    ) -> Self {
        Self::new_for_run(
            root,
            identity,
            replacement_resume_digest,
            attached,
            format!("host-{}", uuid::Uuid::new_v4()),
        )
    }

    /// The same host, with the run id the *authorization* names.
    ///
    /// A managed identity is verified against `authorization.client_run_id ==
    /// host_run_id`, so a random per-host id could never be authorized by the
    /// guardian. Embedded hosts keep the random id: nothing ties them to a lease.
    pub fn new_for_run(
        root: Arc<ThreadRoot>,
        identity: SessionIdentity,
        replacement_resume_digest: Option<String>,
        attached: SessionContainerAttachment,
        host_run_id: String,
    ) -> Self {
        Self {
            root,
            identity,
            replacement_resume_digest,
            host_run_id,
            attached,
            live: Mutex::new(None),
            unclosed: Mutex::new(None),
        }
    }

    pub fn attached(&self) -> &SessionContainerAttachment {
        &self.attached
    }

    pub fn root(&self) -> &Arc<ThreadRoot> {
        &self.root
    }

    /// (Re)establishes the binding, reusing a still-valid one.
    ///
    /// Everything the daemon needs to decide "same live owner" or "genuine
    /// replacement" goes into the request: the current binding when we still have
    /// one, and the stop proofs when we do not.
    pub fn prepare(&mut self, operation_id: &str) -> Result<(), SessionClientError> {
        let current = self
            .live
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let handle = self.root.handle().map_err(SessionClientError::Transport)?;
        let request = PrepareSession {
            root: SessionRef {
                session_id: self.attached.session_id.clone(),
                root_id: self.attached.root_id.clone(),
            },
            operation_id: operation_id.to_string(),
            host_run_id: self.host_run_id.clone(),
            expected_binding_epoch: current
                .as_ref()
                .map(|binding| binding.fence.binding_epoch)
                .unwrap_or_default(),
            identity: self.identity.clone(),
            current_binding: current.clone(),
            replacement_resume_digest: self.replacement_resume_digest.clone(),
            // Stop proofs are collected by the host that is replacing a driver.
            // This host has never registered an execution of its own, so it has
            // nothing it can honestly prove -- claiming otherwise would be a lie
            // about a process it did not observe.
            stopped: Vec::new(),
        };
        self.apply_prepared(&handle, request)
    }

    fn apply_prepared(
        &mut self,
        handle: &RuntimeHandle,
        request: PrepareSession,
    ) -> Result<(), SessionClientError> {
        let response = send_request(
            &handle.socket_path,
            IpcRequest::PrepareSessionDelegation {
                request: request.clone(),
            },
        )
        .map_err(|error| self.transport_failure(&error))?;
        let IpcResponse::SessionPrepared { binding } = response else {
            return Err(failure_from_response(response));
        };
        // A prepared binding is adopted only if it describes the container this
        // host attached. Anything else would silently bind us to another session.
        if binding.fence.root != request.root {
            return Err(SessionClientError::Protocol(
                "the daemon prepared a binding for another container".to_string(),
            ));
        }
        *self
            .live
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(binding);
        Ok(())
    }

    /// Opens one turn and returns the client the tools run under.
    pub fn begin_turn(&mut self, turn_id: &str) -> Result<SessionTurnClient, SessionClientError> {
        if let Some(unclosed) = self
            .unclosed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        {
            // Refusing beats opening a second turn: the previous one's tools could
            // still be mid-call, and the daemon would replace it underneath them.
            return Err(SessionClientError::Rejected(SessionFailure::new(
                SessionFailureCode::AdmissionClosed,
                format!("turn {unclosed} was never proven closed"),
            )));
        }
        if self
            .live
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_none()
        {
            self.prepare(turn_id)?;
        }
        let binding = self
            .live
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .expect("a binding was just prepared");
        let handle = self.root.handle().map_err(SessionClientError::Transport)?;
        let response = send_request(
            &handle.socket_path,
            IpcRequest::BeginSessionTurn {
                binding: binding.clone(),
                turn_id: turn_id.to_string(),
            },
        )
        .map_err(|error| self.transport_failure(&error))?;
        let IpcResponse::SessionTurnOpened { turn } = response else {
            return Err(failure_from_response(response));
        };
        // The turn's own fence must agree with the binding it was opened under,
        // or the daemon answered about a different generation than the one we
        // will send writes to.
        if turn.fence != binding.fence || turn.turn_id != turn_id {
            return Err(SessionClientError::Protocol(
                "the daemon opened a turn outside the prepared binding".to_string(),
            ));
        }
        Ok(SessionTurnClient::new(handle, turn))
    }

    /// Closes a turn. The local side closes first, always.
    ///
    /// A transport failure here leaves the turn *unknown*, not open: the host
    /// refuses to begin another turn until it is proven closed, because that is
    /// the only safe reading of "we could not tell".
    pub fn finish_turn(&mut self, turn: &SessionTurnClient) -> Result<(), SessionClientError> {
        turn.close_locally();
        let handle = turn.handle();
        match send_request(
            &handle.socket_path,
            IpcRequest::EndSessionTurn {
                turn: turn.context().clone(),
            },
        ) {
            Ok(IpcResponse::SessionTurnClosed) => {
                *self
                    .unclosed
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
                Ok(())
            }
            Ok(response) => {
                // A refusal is still a decision about this turn; the local close
                // stands, and the caller learns the daemon did not confirm it.
                *self
                    .unclosed
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                    Some(turn.context().turn_id.clone());
                Err(failure_from_response(response))
            }
            Err(error) => {
                *self
                    .unclosed
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                    Some(turn.context().turn_id.clone());
                Err(self.transport_failure(&error))
            }
        }
    }

    /// Whether a turn was closed locally but the daemon never confirmed it.
    pub fn has_unconfirmed_turn(&self) -> bool {
        self.unclosed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some()
    }

    /// Registers this turn's execution before it does any work.
    ///
    /// Over the host control stream, never a business command: the daemon records
    /// the row before answering, so a host that dies leaves a `starting` row that
    /// blocks the next driver instead of passing for "never started".
    pub fn register_execution(
        &mut self,
        turn_id: &str,
    ) -> Result<ExecutionIdentity, SessionClientError> {
        let binding = self.binding()?;
        let handle = self.root.handle().map_err(SessionClientError::Transport)?;
        let mut control = HostControl::open(&handle.socket_path, binding)?;
        match control.request(SessionHostControlRequest::Register {
            turn_id: turn_id.to_string(),
        })? {
            IpcResponse::SessionHostExecutionRegistered { execution } => Ok(execution),
            other => Err(failure_from_response(other)),
        }
    }

    /// Reports the stop proof for an execution this stream registered.
    ///
    /// Takes `&self`: reporting a stop is not a change to the host's own identity,
    /// and a caller often holds the host only behind a shared reference while the
    /// turn it just joined is being retired.
    pub fn record_stopped(
        &self,
        evidence: yi_agent_lifecycle::foreground::StopEvidence,
    ) -> Result<(), SessionClientError> {
        let binding = self.binding()?;
        let handle = self.root.handle().map_err(SessionClientError::Transport)?;
        let mut control = HostControl::open(&handle.socket_path, binding)?;
        match control.request(SessionHostControlRequest::Stopped {
            evidence: Box::new(evidence),
        })? {
            IpcResponse::SessionHostStopRecorded => Ok(()),
            other => Err(failure_from_response(other)),
        }
    }

    /// The fence of the binding this host currently holds.
    ///
    /// A host that reports its own execution's stop has to name the fence it is
    /// bound under, and only the host knows it: the caller observed a join, not a
    /// generation. `None` means nothing is prepared, so there is nothing to retire.
    pub fn binding_fence(&self) -> Option<SessionFence> {
        self.live
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .map(|binding| binding.fence.clone())
    }

    fn binding(&self) -> Result<SessionBinding, SessionClientError> {
        self.live
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .ok_or_else(|| {
                SessionClientError::Protocol("the session host is not prepared".to_string())
            })
    }

    /// Turns a transport error into the honest answer for this layer.
    ///
    /// A liveness failure is *not* retried: every request that reaches here changes
    /// daemon state or mints a capability, and a lost reply means it may already
    /// have happened. The message says so, so a model checks before trying again.
    fn transport_failure(&self, error: &yi_agent_store::ipc::IpcError) -> SessionClientError {
        if is_liveness_error(error) {
            return SessionClientError::Transport(
                "the runtime connection failed and the request was NOT retried: it may \
                 already have been applied. Re-prepare the session before retrying."
                    .to_string(),
            );
        }
        SessionClientError::Transport(error.to_string())
    }
}

/// The host control stream: one connection, opened with the binding capability.
struct HostControl {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    request_id: String,
}

impl HostControl {
    fn open(
        socket_path: &std::path::Path,
        binding: SessionBinding,
    ) -> Result<Self, SessionClientError> {
        let mut stream = UnixStream::connect(socket_path)
            .map_err(|error| SessionClientError::Transport(error.to_string()))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .map_err(|error| SessionClientError::Transport(error.to_string()))?;
        let request_id = format!("host-control-{}", uuid::Uuid::new_v4());
        let envelope = RequestEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request_id: request_id.clone(),
            command: IpcRequest::OpenSessionHostControl { binding },
        };
        write_envelope(&mut stream, &envelope)?;
        let mut reader = BufReader::new(
            stream
                .try_clone()
                .map_err(|error| SessionClientError::Transport(error.to_string()))?,
        );
        let ready = read_envelope(&mut reader, &request_id)?;
        match ready {
            IpcResponse::SessionHostControlReady => Ok(Self {
                writer: stream,
                reader,
                request_id,
            }),
            other => Err(failure_from_response(other)),
        }
    }

    fn request(
        &mut self,
        request: SessionHostControlRequest,
    ) -> Result<IpcResponse, SessionClientError> {
        let frame = SessionHostControlFrame {
            request_id: format!("{}-{}", self.request_id, next_sequence()),
            request,
        };
        let bytes = serde_json::to_vec(&frame)
            .map_err(|error| SessionClientError::Protocol(error.to_string()))?;
        self.writer
            .write_all(&bytes)
            .and_then(|()| self.writer.write_all(b"\n"))
            .and_then(|()| self.writer.flush())
            .map_err(|error| SessionClientError::Transport(error.to_string()))?;
        // The daemon answers each control frame in place; a read failure means the
        // stream is gone, which is not retryable for a state-changing frame.
        read_envelope(&mut self.reader, &self.request_id).map_err(|error| match error {
            SessionClientError::Protocol(_) => error,
            SessionClientError::Rejected(failure) => SessionClientError::Rejected(failure),
            SessionClientError::Transport(message) => SessionClientError::Transport(message),
        })
    }
}

fn next_sequence() -> u64 {
    use std::sync::atomic::AtomicU64;
    static SEQUENCE: AtomicU64 = AtomicU64::new(1);
    SEQUENCE.fetch_add(1, Ordering::Relaxed)
}

fn write_envelope(
    stream: &mut UnixStream,
    envelope: &RequestEnvelope,
) -> Result<(), SessionClientError> {
    let frame = serde_json::to_vec(envelope)
        .map_err(|error| SessionClientError::Protocol(error.to_string()))?;
    stream
        .write_all(&frame)
        .and_then(|()| stream.write_all(b"\n"))
        .and_then(|()| stream.flush())
        .map_err(|error| SessionClientError::Transport(error.to_string()))
}

fn read_envelope(
    reader: &mut BufReader<UnixStream>,
    request_id: &str,
) -> Result<IpcResponse, SessionClientError> {
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|error| SessionClientError::Transport(error.to_string()))?;
    if line.trim().is_empty() {
        return Err(SessionClientError::Transport(
            "the daemon closed the host control stream".to_string(),
        ));
    }
    let envelope: ResponseEnvelope = serde_json::from_str(&line)
        .map_err(|error| SessionClientError::Protocol(error.to_string()))?;
    if envelope.request_id != request_id || envelope.protocol_version != PROTOCOL_VERSION {
        return Err(SessionClientError::Protocol(
            "the host control stream answered another request".to_string(),
        ));
    }
    Ok(envelope.result)
}

/// One run's capabilities, captured when the turn opened.
///
/// Cloned into the tool table, never refreshed afterwards. That is what makes a
/// delayed old tool harmless: it holds a closed turn and a socket, and nothing it
/// can do reaches the newer turn.
#[derive(Clone)]
pub struct SessionTurnClient {
    handle: RuntimeHandle,
    turn: SessionTurn,
    closed: Arc<AtomicBool>,
}

impl SessionTurnClient {
    pub fn new(handle: RuntimeHandle, turn: SessionTurn) -> Self {
        Self {
            handle,
            turn,
            closed: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn context(&self) -> &SessionTurn {
        &self.turn
    }

    /// The fixed handle for this turn: this socket, this root, this turn secret.
    ///
    /// The capability is the turn secret, never the binding secret -- a tool must
    /// not be able to open a host control stream.
    pub fn handle(&self) -> RuntimeHandle {
        RuntimeHandle {
            socket_path: self.handle.socket_path.clone(),
            workspace_root: self.handle.workspace_root.clone(),
            session_id: self.handle.session_id.clone(),
            task_id: self.handle.task_id.clone(),
            capability: self.turn.capability.expose().to_string(),
        }
    }

    pub fn socket_path(&self) -> &PathBuf {
        &self.handle.socket_path
    }

    /// Marks this turn unusable for new calls. Idempotent.
    pub fn close_locally(&self) {
        self.closed.store(true, Ordering::Release);
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Whether a new call may proceed.
    ///
    /// Purely local: the daemon re-checks the turn on every write, and this is the
    /// cheaper first gate so a closed turn is refused without a round trip.
    pub fn ensure_open(&self) -> Result<(), SessionClientError> {
        if self.is_closed() {
            return Err(SessionClientError::Rejected(SessionFailure::new(
                SessionFailureCode::StaleTurn,
                format!("turn {} was closed locally", self.turn.turn_id),
            )));
        }
        Ok(())
    }

    /// Sends one read-only request, even after this turn was closed.
    ///
    /// Reads are deliberately not gated on "still open": a run that ended must
    /// still be able to read the results of the children it started, otherwise a
    /// finished turn would make its own work unreadable. The daemon re-checks the
    /// secret against the live binding, so a replaced driver's turn still fails.
    pub fn send_read(
        &self,
        build: impl Fn(&RuntimeHandle, &SessionTurn) -> IpcRequest,
    ) -> Result<IpcResponse, SessionClientError> {
        let handle = self.handle();
        let request = build(&handle, &self.turn);
        match send_request(&handle.socket_path, request) {
            Ok(response) => Ok(response),
            Err(error) => Err(SessionClientError::Transport(error.to_string())),
        }
    }

    /// Sends one business request as this turn.
    pub fn send(
        &self,
        build: impl Fn(&RuntimeHandle, &SessionTurn) -> IpcRequest,
    ) -> Result<IpcResponse, SessionClientError> {
        self.ensure_open()?;
        let handle = self.handle();
        let request = build(&handle, &self.turn);
        match send_request(&handle.socket_path, request) {
            Ok(response) => Ok(response),
            Err(error) if is_liveness_error(&error) => Err(SessionClientError::Transport(
                "the runtime connection failed and the request was NOT retried: it may \
                 already have been applied."
                    .to_string(),
            )),
            Err(error) => Err(SessionClientError::Transport(error.to_string())),
        }
    }
}
