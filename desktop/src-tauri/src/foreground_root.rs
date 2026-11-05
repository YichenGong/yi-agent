//! Confirmed foreground-root recovery for the desktop shell.
//!
//! A foreground conversation can be settled by mistake: the watchdog once treated
//! the long-lived container as a background worker attempt and closed it. Repairing
//! one is destructive -- it freezes the root, proves every execution stopped, and
//! only then commits a successor attempt -- so it must not be reachable from a
//! model, a tool, or a relayed JSON-RPC caller. This module is therefore a set of
//! Tauri commands the *local* window calls, and nothing else.
//!
//! Three rules shape it:
//!
//! * **The shell speaks for itself.** Recovery is owner-bound: the guardian checks
//!   that the asking grant holds a live `Business` lease. So the calls go over the
//!   shell's own control channel using the lease it already holds, exactly like the
//!   restart surface -- not through the sidecar, which has no grant and must not
//!   acquire one on the App's behalf.
//! * **Looking never stops anything.** `inspect` is read-only at the daemon, so a
//!   window that polls it cannot interrupt the conversation the user is still
//!   deciding about.
//! * **A lost answer is resolved by asking, never by minting.** Every destructive
//!   step is keyed by an `operation_id` the caller chose *before* the click, so a
//!   timed-out confirm is resolved by querying that same id rather than by preparing
//!   a second recovery.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use tauri::Emitter;
use yi_agent_lifecycle::foreground::{
    ForegroundErrorCode, ForegroundFence, RecoveryImpact, RecoveryOperationStatus, RecoveryRequest,
    RootRef, SecretCapability,
};
use yi_agent_lifecycle::types::RuntimeScope;

use crate::runtime_lifecycle::{QuitState, SWITCH_BUDGET};

/// One control frame's ceiling for the read-only steps.
///
/// The same budget the rest of the shell's read-only surface uses: a wedged demon
/// should make the panel say "unknown" quickly, and the UI polls again anyway.
const READ_BUDGET: Duration = yi_agent_lifecycle::CONTROL_REQUEST_TIMEOUT;

/// Errors this surface reports to the window.
///
/// A structured code rather than a message: the WebView maps the code to one
/// sentence, so an upstream error string (which has carried paths and secrets
/// before) never reaches the DOM, the transcript or a log.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum HostRecoveryError {
    /// The runtime answered, and refused for a reason the user can act on.
    Rejected {
        code: ForegroundErrorCode,
        reason: String,
    },
    /// The socket or the process is gone. Distinct from a refusal: a retry is
    /// meaningful, and telling them apart is the difference between "try again"
    /// and "this will never work".
    Transport { reason: String },
    /// The runtime spoke something this build does not understand.
    Protocol { reason: String },
    /// The named project is not open in this app, so there is no runtime to ask.
    SessionNotFound { reason: String },
}

impl HostRecoveryError {
    fn from_io(stage: &'static str, error: io::Error) -> Self {
        // `Unsupported` is a refusal, not a transport failure: the build or the
        // runtime simply does not serve recovery, and a caller retrying it would
        // spin.
        match error.kind() {
            io::ErrorKind::Unsupported => Self::Rejected {
                code: ForegroundErrorCode::Unsupported,
                reason: format!("{stage}: this runtime does not serve foreground recovery"),
            },
            io::ErrorKind::PermissionDenied
            | io::ErrorKind::InvalidInput
            | io::ErrorKind::NotFound => Self::Rejected {
                code: ForegroundErrorCode::AuthorityDenied,
                reason: format!("{stage}: the recovery was refused"),
            },
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => Self::Transport {
                reason: format!("{stage}: the runtime did not answer in time"),
            },
            io::ErrorKind::InvalidData => Self::Protocol {
                reason: format!("{stage}: the runtime answered something unexpected"),
            },
            _ => Self::Transport {
                reason: format!("{stage}: the runtime could not be reached"),
            },
        }
    }

    fn session_missing(scope_key: &str) -> Self {
        Self::SessionNotFound {
            reason: format!(
                "project `{scope_key}` is not open in this app, so it has no runtime to talk to"
            ),
        }
    }
}

/// The fence a caller must name to look at or repair one root.
///
/// Every field comes from what the daemon already persisted: a caller that invented
/// an epoch would be describing a conversation that does not exist, and the daemon
/// refuses a stale fence rather than acting on the wrong attempt.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryTarget {
    pub session_id: String,
    pub root_id: String,
    pub attempt_id: String,
    pub generation: String,
    pub binding_epoch: u64,
    pub driver_epoch: u64,
}

impl RecoveryTarget {
    fn fence(&self) -> ForegroundFence {
        ForegroundFence {
            root: RootRef {
                session_id: self.session_id.clone(),
                root_id: self.root_id.clone(),
                attempt_id: self.attempt_id.clone(),
            },
            generation: self.generation.clone(),
            binding_epoch: self.binding_epoch,
            driver_epoch: self.driver_epoch,
        }
    }
}

/// What a freeze would stop, as the window shows it.
///
/// Only counts and stable ids: the impact is a description, and a description that
/// carried a capability would be a secret in the DOM.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryImpactView {
    pub root_id: String,
    pub attempt_id: String,
    pub stopped_execution_ids: Vec<String>,
    pub blocked_execution_ids: Vec<String>,
    pub child_ids: Vec<String>,
    pub version: u64,
    pub reason: String,
}

impl From<RecoveryImpact> for RecoveryImpactView {
    fn from(impact: RecoveryImpact) -> Self {
        Self {
            root_id: impact.root.root_id,
            attempt_id: impact.root.attempt_id,
            stopped_execution_ids: impact.stopped_execution_ids,
            blocked_execution_ids: impact.blocked_execution_ids,
            child_ids: impact.child_ids,
            version: impact.version,
            reason: impact.reason,
        }
    }
}

/// A prepared recovery, as the window shows it.
///
/// `token` is `null` while any execution is unproven: a blocked preparation must not
/// look confirmable, and inventing an expiry for it would be a promise it cannot
/// keep. The token lives only in the confirming component's memory; it is never
/// written to preferences, the transcript or localStorage.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryPreviewView {
    pub operation_id: String,
    pub token: Option<String>,
    pub blocked: Vec<String>,
    pub expires_at: Option<String>,
    pub impact: RecoveryImpactView,
}

/// A committed recovery. Deliberately free of every secret: a replay must never
/// hand out authority twice, so the successor is named but not authorized -- a new
/// driver bind is what obtains the current narrow capability.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryResultView {
    pub operation_id: String,
    pub previous_root_id: String,
    pub previous_attempt_id: String,
    pub successor_attempt_id: String,
    pub binding_epoch: u64,
    pub driver_epoch: u64,
}

/// One operation's observable state.
///
/// A tagged union because `preparing` and `committed` mean opposite things to a
/// user: only `committed` may ever be rendered as success.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "phase", rename_all = "camelCase")]
pub enum RecoveryOperationView {
    #[serde(rename = "preparing")]
    Preparing { operation_id: String },
    #[serde(rename = "needsPreview")]
    NeedsPreview { operation_id: String },
    #[serde(rename = "blocked")]
    Blocked {
        operation_id: String,
        impact: RecoveryImpactView,
    },
    #[serde(rename = "ready")]
    Ready {
        operation_id: String,
        preview: RecoveryPreviewView,
    },
    #[serde(rename = "committed")]
    Committed { result: RecoveryResultView },
}

impl RecoveryOperationView {
    /// Shapes one daemon status into the view, given the operation it is about.
    ///
    /// The operation id is passed in rather than read from the status because
    /// `Preparing` and `NeedsPreview` carry nothing: a view that lost the id would
    /// leave the window unable to poll the same operation it is showing.
    fn from_status(operation_id: &str, status: RecoveryOperationStatus) -> Self {
        match status {
            RecoveryOperationStatus::Preparing => Self::Preparing {
                operation_id: operation_id.to_string(),
            },
            RecoveryOperationStatus::NeedsPreview => Self::NeedsPreview {
                operation_id: operation_id.to_string(),
            },
            RecoveryOperationStatus::Blocked(impact) => Self::Blocked {
                operation_id: operation_id.to_string(),
                impact: impact.into(),
            },
            RecoveryOperationStatus::Ready(preview) => Self::Ready {
                operation_id: operation_id.to_string(),
                preview: RecoveryPreviewView {
                    operation_id: preview.request.operation_id.clone(),
                    token: preview
                        .confirmation_token
                        .as_ref()
                        .map(|token| token.expose().to_string()),
                    blocked: preview.impact.blocked_execution_ids.clone(),
                    expires_at: preview.expires_at.clone(),
                    impact: preview.impact.into(),
                },
            },
            RecoveryOperationStatus::Committed(result) => Self::Committed {
                result: RecoveryResultView {
                    operation_id: result.operation_id,
                    previous_root_id: result.previous.root_id,
                    previous_attempt_id: result.previous.attempt_id,
                    successor_attempt_id: result.successor.attempt_id,
                    binding_epoch: result.binding_epoch,
                    driver_epoch: result.driver_epoch,
                },
            },
        }
    }
}

/// Resolves the scope and runs one blocking control call off the event loop.
///
/// Recovery's client is a blocking socket client, so it goes through
/// `spawn_blocking` with the same bounded timeout the rest of the control surface
/// uses: a wedged daemon delays the answer only until the budget runs out.
async fn with_lease<T, F>(
    state: &QuitState,
    scope_key: &str,
    budget: Duration,
    call: F,
) -> Result<T, HostRecoveryError>
where
    T: Send + 'static,
    F: FnOnce(
            Arc<std::sync::Mutex<yi_agent_lifecycle::client::FrontendLease>>,
            RuntimeScope,
        ) -> io::Result<T>
        + Send
        + 'static,
{
    let scope = state
        .manager
        .known_scopes()
        .into_iter()
        .find(|scope| scope.key == scope_key)
        .ok_or_else(|| HostRecoveryError::session_missing(scope_key))?;
    let lease = state
        .manager
        .lease_for_recovery(&scope)
        .map_err(|error| HostRecoveryError::from_io("lease", error))?;
    let state_scope = scope.clone();
    match tokio::time::timeout(
        budget,
        tokio::task::spawn_blocking(move || call(lease, state_scope)),
    )
    .await
    {
        Ok(Ok(result)) => result.map_err(|error| HostRecoveryError::from_io("recovery", error)),
        Ok(Err(join)) => Err(HostRecoveryError::Transport {
            reason: format!("recovery task failed: {join}"),
        }),
        Err(_) => Err(HostRecoveryError::Transport {
            reason: "no budget remains for the recovery request".to_string(),
        }),
    }
}

/// Reads what a recovery would stop. Read-only at every hop.
#[tauri::command]
pub async fn foreground_root_recovery_inspect(
    state: tauri::State<'_, QuitState>,
    scope_key: String,
    target: RecoveryTarget,
) -> Result<RecoveryImpactView, HostRecoveryError> {
    let fence = target.fence();
    let impact = with_lease(&state, &scope_key, READ_BUDGET, move |lease, _scope| {
        let lease = lease
            .lock()
            .map_err(|_| io::Error::other("the recovery lease is poisoned"))?;
        let deadline = yi_agent_lifecycle::deadline::Deadline::after(READ_BUDGET);
        lease
            .inspect_root_recovery(&fence, &deadline)
            .map_err(|error| error)
    })
    .await?;
    Ok(impact.into())
}

/// Freezes and stops, then reports whether a confirmation may be issued.
#[tauri::command]
pub async fn foreground_root_recovery_prepare(
    state: tauri::State<'_, QuitState>,
    scope_key: String,
    target: RecoveryTarget,
    operation_id: String,
) -> Result<RecoveryOperationView, HostRecoveryError> {
    if operation_id.trim().is_empty() {
        return Err(HostRecoveryError::Rejected {
            code: ForegroundErrorCode::OperationConflict,
            reason: "a prepare needs the operation id chosen before the click".to_string(),
        });
    }
    let request = RecoveryRequest {
        expected: target.fence(),
        operation_id: operation_id.clone(),
    };
    let budget = SWITCH_BUDGET;
    let status = with_lease(&state, &scope_key, budget, move |lease, _scope| {
        let lease = lease
            .lock()
            .map_err(|_| io::Error::other("the recovery lease is poisoned"))?;
        let deadline = yi_agent_lifecycle::deadline::Deadline::after(budget);
        lease.prepare_root_recovery(&request, &deadline)
    })
    .await?;
    Ok(match status {
        Some(status) => RecoveryOperationView::from_status(&operation_id, status),
        // The daemon has no record of the operation it was just asked to create.
        // Reporting any state here would describe a prepare that never happened.
        None => {
            return Err(HostRecoveryError::Protocol {
                reason: "the runtime prepared no operation".to_string(),
            });
        }
    })
}

/// Commits a prepared recovery with the token its prepare returned.
#[tauri::command]
pub async fn foreground_root_recovery_confirm(
    app: tauri::AppHandle,
    state: tauri::State<'_, QuitState>,
    scope_key: String,
    target: RecoveryTarget,
    operation_id: String,
    token: String,
) -> Result<RecoveryOperationView, HostRecoveryError> {
    if token.trim().is_empty() {
        return Err(HostRecoveryError::Rejected {
            code: ForegroundErrorCode::ConfirmationRequired,
            reason: "a confirmation needs the token from the preview it was shown".to_string(),
        });
    }
    if operation_id.trim().is_empty() {
        return Err(HostRecoveryError::Rejected {
            code: ForegroundErrorCode::OperationConflict,
            reason: "a confirmation needs the operation id chosen before the click".to_string(),
        });
    }
    let request = RecoveryRequest {
        expected: target.fence(),
        operation_id: operation_id.clone(),
    };
    let secret = SecretCapability::new(token);
    let budget = SWITCH_BUDGET;
    let status = with_lease(&state, &scope_key, budget, move |lease, _scope| {
        let lease = lease
            .lock()
            .map_err(|_| io::Error::other("the recovery lease is poisoned"))?;
        let deadline = yi_agent_lifecycle::deadline::Deadline::after(budget);
        lease.confirm_root_recovery(&request, &secret, &deadline)
    })
    .await?;
    let view = match status {
        Some(status) => RecoveryOperationView::from_status(&operation_id, status),
        None => {
            return Err(HostRecoveryError::Protocol {
                reason: "the runtime has no record of this recovery operation".to_string(),
            });
        }
    };
    announce_commit(&app, &view);
    Ok(view)
}

/// Reads one operation's stored state. `Ok(null)` means no such operation.
#[tauri::command]
pub async fn foreground_root_recovery_operation(
    state: tauri::State<'_, QuitState>,
    scope_key: String,
    target: RecoveryTarget,
    operation_id: String,
) -> Result<Option<RecoveryOperationView>, HostRecoveryError> {
    let root = RootRef {
        session_id: target.session_id.clone(),
        root_id: target.root_id.clone(),
        attempt_id: target.attempt_id.clone(),
    };
    let asked = operation_id.clone();
    let status = with_lease(&state, &scope_key, READ_BUDGET, move |lease, _scope| {
        let lease = lease
            .lock()
            .map_err(|_| io::Error::other("the recovery lease is poisoned"))?;
        let deadline = yi_agent_lifecycle::deadline::Deadline::after(READ_BUDGET);
        lease.root_recovery_operation(&root, &asked, &deadline)
    })
    .await?;
    Ok(status.map(|status| RecoveryOperationView::from_status(&operation_id, status)))
}

/// Announces that a recovery committed, so open views can refresh.
///
/// Emitted from the confirming command rather than polled into existence: a window
/// that does not have the operation open must still learn that the conversation it
/// is showing has a new attempt, or it would keep reporting activity under a driver
/// that no longer holds the root.
pub fn announce_commit(app: &tauri::AppHandle, view: &RecoveryOperationView) {
    if let RecoveryOperationView::Committed { result } = view {
        let _ = app.emit("foreground-root-recovered", result.clone());
    }
}
