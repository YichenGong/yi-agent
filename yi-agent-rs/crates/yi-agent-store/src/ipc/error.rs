use super::*;

#[derive(Debug, Error)]
pub enum IpcError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Repository(#[from] crate::repository::RepositoryError),
    #[error(transparent)]
    Runtime(#[from] RuntimeCoordinatorError),
    #[error("a daemon is already running for {path}")]
    AlreadyRunning { path: PathBuf },
    /// The database was written by a build this one does not understand. A
    /// user-visible state, not an internal fault: the fix is to upgrade (or stop)
    /// the other build, and the daemon must not write to it in the meantime.
    #[error("incompatible database schema: {0}")]
    SchemaIncompatible(String),
    #[error("plugin {plugin} is not running or does not declare a query socket")]
    PluginUnavailable { plugin: String },
    #[error("plugin {plugin} did not answer within {seconds}s")]
    PluginTimeout { plugin: String, seconds: u64 },
    #[error("plugin {plugin} refused the query")]
    PluginRefused { plugin: String },
    #[error("IPC frame exceeds {MAX_FRAME_BYTES} bytes")]
    FrameTooLarge,
    #[error("IPC response frame is truncated after {received} bytes (the peer closed mid-frame)")]
    TruncatedFrame { received: usize },
    #[error("timed out writing response frame after {written} of {total} bytes")]
    FrameWriteTimeout { written: usize, total: usize },
    #[error("daemon listener thread panicked during shutdown")]
    ListenerPanicked,
    #[error(
        "socket path {path} exceeds this platform's {limit}-byte limit; \
         shorten the project path or set YI_AGENT_RUNTIME_DIR to a short directory"
    )]
    SocketPathTooLong { path: String, limit: usize },
    /// The daemon answered with an error frame. The code is its stable
    /// category and the message is whatever it chose to disclose; both are
    /// already sanitized at the wire boundary.
    #[error("daemon returned {code}: {message}")]
    Remote { code: IpcErrorCode, message: String },
}

pub(crate) fn request_id_from_frame(frame: &[u8]) -> String {
    request_id_from_json(frame).unwrap_or_else(|| MISSING_REQUEST_ID.into())
}

pub(crate) fn request_id_from_json(frame: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(frame)
        .ok()
        .and_then(|value| value.get("request_id")?.as_str().map(str::to_owned))
}

pub(crate) fn request_id_from_prefix(prefix: &[u8]) -> String {
    request_id_from_json(prefix)
        .or_else(|| json_string_field(prefix, b"\"request_id\""))
        .unwrap_or_else(|| MISSING_REQUEST_ID.into())
}

pub(crate) fn json_string_field(frame: &[u8], key: &[u8]) -> Option<String> {
    for key_start in frame
        .windows(key.len())
        .enumerate()
        .filter_map(|(index, bytes)| (bytes == key).then_some(index))
    {
        let mut value_start = key_start + key.len();
        while frame.get(value_start).is_some_and(u8::is_ascii_whitespace) {
            value_start += 1;
        }
        if frame.get(value_start) != Some(&b':') {
            continue;
        }
        value_start += 1;
        while frame.get(value_start).is_some_and(u8::is_ascii_whitespace) {
            value_start += 1;
        }
        if frame.get(value_start) != Some(&b'\"') {
            continue;
        }
        let mut escaped = false;
        for value_end in value_start + 1..frame.len() {
            match frame[value_end] {
                b'\\' if !escaped => escaped = true,
                b'\"' if !escaped => {
                    if let Ok(value) = serde_json::from_slice(&frame[value_start..=value_end]) {
                        return Some(value);
                    }
                    break;
                }
                _ => escaped = false,
            }
        }
    }
    None
}

pub(crate) fn error_response(error: &IpcError) -> IpcResponse {
    IpcResponse::Error {
        code: ipc_error_code(error),
        message: ipc_error_message(error),
    }
}

/// Records why a request failed.
///
/// `error_response` deliberately strips the cause so the wire contract never
/// leaks database, environment, or implementation detail, and most variants map
/// to a bare `internal`. Without this the daemon is silently wrong: a wedged
/// runtime that answers every request with `internal` leaves no trace, and the
/// client can only report the opaque code. The full error goes here; the client
/// still gets the stable code.
pub(crate) fn log_request_failure(request_id: &str, error: &IpcError) {
    tracing::error!(
        request_id,
        error = %error,
        "request failed; the client sees only the stable error code"
    );
}

pub(crate) fn ipc_error_message(error: &IpcError) -> Option<String> {
    match error {
        IpcError::Runtime(RuntimeCoordinatorError::Supervisor(message)) => Some(message.clone()),
        IpcError::PluginUnavailable { plugin } => Some(format!("plugin {plugin} is not available")),
        IpcError::PluginTimeout { plugin, seconds } => {
            Some(format!("plugin {plugin} timed out after {seconds}s"))
        }
        IpcError::Runtime(RuntimeCoordinatorError::Spawn(
            yi_agent_core::subagent::supervisor::SpawnError::DirectChildLimitReached { limit },
        )) => Some(format!("an agent may have at most {limit} direct children")),
        // The error's own Display already names the settled state, which is what
        // tells the model whether retrying could ever help.
        IpcError::Runtime(error @ RuntimeCoordinatorError::ParentSettled { .. }) => {
            Some(error.to_string())
        }
        // A fork rejection is the caller's mistake (unknown, incomplete, or
        // mismatched token), so the reason is safe to hand back verbatim rather
        // than leaving the client with only a bare code.
        IpcError::Runtime(RuntimeCoordinatorError::ForkUpload(reason)) => Some(reason.clone()),
        IpcError::Runtime(RuntimeCoordinatorError::ForkTooLarge { total, max }) => Some(format!(
            "fork payload of {total} bytes exceeds the {max}-byte limit"
        )),
        // Carries a stable prefix plus the reason and the path, so the tool layer
        // can rebuild a structured rejection the model can act on.
        IpcError::Runtime(error @ RuntimeCoordinatorError::CodingRequiresGit { .. }) => {
            Some(error.to_string())
        }
        _ => None,
    }
}

pub(crate) fn ipc_error_code(error: &IpcError) -> IpcErrorCode {
    match error {
        IpcError::Repository(error) => repository_error_code(error),
        IpcError::Runtime(RuntimeCoordinatorError::SessionNotFound(_)) => IpcErrorCode::NotFound,
        IpcError::Runtime(RuntimeCoordinatorError::Repository(error)) => {
            repository_error_code(error)
        }
        IpcError::Runtime(RuntimeCoordinatorError::Spawn(
            yi_agent_core::subagent::supervisor::SpawnError::ParentNotFound,
        )) => IpcErrorCode::NotFound,
        IpcError::Runtime(RuntimeCoordinatorError::Supervisor(_))
        | IpcError::Runtime(RuntimeCoordinatorError::Spawn(_))
        | IpcError::Runtime(RuntimeCoordinatorError::ParentSettled { .. }) => {
            IpcErrorCode::InvalidState
        }
        IpcError::Runtime(RuntimeCoordinatorError::ResidentCapacityExhausted) => {
            IpcErrorCode::RateLimited
        }
        IpcError::Runtime(RuntimeCoordinatorError::Draining) => IpcErrorCode::InvalidState,
        IpcError::Runtime(RuntimeCoordinatorError::AuthorityDenied(_)) => {
            IpcErrorCode::AuthorityDenied
        }
        // A bad fork token or an oversized payload is the caller's error, not a
        // daemon fault: surface it as validation so the client can react.
        IpcError::Runtime(RuntimeCoordinatorError::CodingRequiresGit { .. })
        | IpcError::Runtime(RuntimeCoordinatorError::ForkTooLarge { .. })
        | IpcError::Runtime(RuntimeCoordinatorError::ForkUpload(_)) => IpcErrorCode::Validation,
        IpcError::Io(error) if error.kind() == std::io::ErrorKind::InvalidInput => {
            IpcErrorCode::Validation
        }
        // Transport-layer failures stay Validation: a truncated or timed-out
        // response is not a daemon fault, and reporting it as Internal sends
        // operators hunting for a crashed daemon that is running fine.
        IpcError::Json(_)
        | IpcError::FrameTooLarge
        | IpcError::TruncatedFrame { .. }
        | IpcError::FrameWriteTimeout { .. } => IpcErrorCode::Validation,
        IpcError::PluginUnavailable { .. } => IpcErrorCode::NotFound,
        IpcError::PluginTimeout { .. } => IpcErrorCode::InvalidState,
        IpcError::SchemaIncompatible(_) => IpcErrorCode::InvalidState,
        IpcError::PluginRefused { .. } => IpcErrorCode::Validation,
        IpcError::Remote { code, .. } => *code,
        _ => IpcErrorCode::Internal,
    }
}

pub(crate) fn repository_error_code(error: &crate::repository::RepositoryError) -> IpcErrorCode {
    match error {
        crate::repository::RepositoryError::TaskNotFound { .. }
        | crate::repository::RepositoryError::MailboxMessageNotFound { .. }
        | crate::repository::RepositoryError::PermissionRequestNotFound { .. } => {
            IpcErrorCode::NotFound
        }
        crate::repository::RepositoryError::PermissionRequestNotPending { .. } => {
            IpcErrorCode::InvalidState
        }
        crate::repository::RepositoryError::DeliveryRequiresParent { .. }
        | crate::repository::RepositoryError::TaskNotReadyForDelivery { .. } => {
            IpcErrorCode::InvalidState
        }
        // A schema outside this build's range is a configuration/version problem
        // the caller must report, not an internal defect: mapping it to `Internal`
        // would send it down the "daemon is broken, restart it" path.
        crate::repository::RepositoryError::SchemaIncompatible { .. } => IpcErrorCode::InvalidState,
        // A foreground rejection is a policy answer, not a daemon fault: the client
        // must see a stable code it can dispatch on (Task 3 turns the typed error
        // into a wire `ForegroundFailure`), so it must not fall into `Internal`.
        crate::repository::RepositoryError::Foreground(_) => IpcErrorCode::InvalidState,
        crate::repository::RepositoryError::Sql(_)
        | crate::repository::RepositoryError::Json(_)
        | crate::repository::RepositoryError::InvalidWorkerRecoveryContext { .. }
        | crate::repository::RepositoryError::InvalidAdmissionCursor { .. }
        | crate::repository::RepositoryError::InvalidWatchdogSnapshot { .. }
        | crate::repository::RepositoryError::InvalidForegroundFact { .. }
        | crate::repository::RepositoryError::UnknownEventKind { .. } => IpcErrorCode::Internal,
    }
}
