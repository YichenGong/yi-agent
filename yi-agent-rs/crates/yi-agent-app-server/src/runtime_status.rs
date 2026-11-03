//! `runtime/status`: the read-only view of the runtime a project is attached to.
//!
//! Three constraints shape this module, and each one rules out the obvious
//! alternative:
//!
//! 1. **It is read-only, and that is checkable.** [`is_read_only_runtime_method`]
//!    answers "may a remote client call this without being able to destroy work?".
//!    The set is closed on purpose: a new method is not read-only because whoever
//!    added it says so, it is read-only because it is listed here.
//! 2. **The security boundary is the socket, not this frame.** The guardian's
//!    `Status` request carries no grant (see `ControlRequest::Status`), so this
//!    module cannot be a way to learn a credential, and does not try to be a way to
//!    authenticate one.
//! 3. **A compatibility answer is a classification, not a boolean.** "Different
//!    build", "cannot do business work" and "we do not know what this is" lead to
//!    three different UI decisions, so [`Compatibility`] reuses the enum the
//!    bring-up path already uses rather than inventing a second vocabulary.
//!
//! The DTOs here exist to be exported to TypeScript (`ts-rs`), which is why they
//! are local mirrors of the lifecycle wire types instead of re-exports: the
//! lifecycle crate deliberately does not link `ts-rs` (the desktop shell links the
//! lifecycle crate, and a second generator in that graph is exactly the kind of
//! weight it guards against).

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};
use ts_rs::TS;
use yi_agent_lifecycle::deadline::Deadline;
use yi_agent_lifecycle::types::{
    ArtifactIdentity, ClientKind, ControlRequest, ControlResponse, ImpactCounts, LeaseMode,
    OperationStatus, RuntimeIdentity, RuntimeStatusSnapshot,
};

/// The methods a remote (websocket/relay) client may call.
///
/// A closed list rather than a prefix test: `runtime/*` is the natural place to put
/// a destructive control next (a restart, a stop), and a prefix test would silently
/// make it remote-callable the day it is added. Adding a name here is a deliberate
/// statement that the method cannot lose a user's work.
pub fn is_read_only_runtime_method(method: &str) -> bool {
    matches!(method, "runtime/status")
}

/// How the running runtime compares to what this frontend expects.
///
/// Mirrors `yi_agent_supervisors::lifecycle::bringup::Compatibility` variant for
/// variant. Redeclared because the app-server cannot export the supervisor type to
/// TypeScript without giving the supervisors a `ts-rs` dependency, and the
/// supervisors carry the store.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
#[serde(rename_all = "camelCase")]
pub enum Compatibility {
    /// The exact artifact this caller expects.
    SameBuild,
    /// A different build that can still serve this caller's business protocol.
    CompatibleMismatch,
    /// Control works, business does not: usable for a controlled switch, not for
    /// running tasks.
    ControlOnly,
    /// An old runtime with no lifecycle guarantees. Usable only if the user
    /// explicitly accepts the exception.
    LegacyTemporaryChoiceRequired,
    /// Not enough verified information. Never business.
    Unknown,
}

/// One connected frontend, as a UI needs it.
///
/// No credential, no epoch, no message text: this is what a banner renders, not
/// what a client authenticates with.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
#[serde(rename_all = "camelCase")]
pub struct ClientSummaryView {
    pub client_run_id: String,
    pub kind: ClientKindDto,
    pub mode: LeaseModeDto,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
#[serde(rename_all = "snake_case")]
pub enum ClientKindDto {
    App,
    Tui,
    Headless,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
#[serde(rename_all = "snake_case")]
pub enum LeaseModeDto {
    Business,
    LeaseOnly,
    WaitingNextGeneration,
}

/// Counts of what a replacement would disturb.
///
/// `complete` is load-bearing: `false` means every number is a lower bound, so a UI
/// must not render `running: 0` as "nothing is running".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
#[serde(rename_all = "camelCase")]
pub struct ImpactCountsView {
    pub clients: u64,
    pub pending_clients: u64,
    pub running: u64,
    pub queued: u64,
    pub plugins: u64,
    pub complete: bool,
}

/// The running runtime's identity, without the credential-bearing half.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
#[serde(rename_all = "camelCase")]
pub struct RuntimeIdentityView {
    pub scope_key: String,
    pub daemon_instance_id: Option<String>,
    pub guardian_build_id: Option<String>,
    pub daemon_build_id: Option<String>,
    pub mode: RuntimeModeDto,
    pub business_protocol: Option<u32>,
    pub daemon_display_version: Option<String>,
    pub guardian_display_version: Option<String>,
    pub capabilities: Vec<String>,
    pub schema_min: u32,
    pub schema_max: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
#[serde(rename_all = "snake_case")]
pub enum RuntimeModeDto {
    Managed,
    LegacyIndependent,
    LegacyEmbedded,
    Unknown,
}

/// The two independent counters a preview's token is bound to.
///
/// Two values rather than one hash: this pair decides whether a confirmation still
/// applies, and a hash could collide into a false match. They are compared for
/// equality, never ordered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
#[serde(rename_all = "camelCase")]
pub struct ImpactRevisionView {
    pub guardian: u64,
    /// `None` means the business daemon did not answer, so the impact is unknown --
    /// which must never be rendered as zero.
    pub coordinator: Option<u64>,
}

/// One thing a replacement would disturb.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
#[serde(rename_all = "camelCase")]
pub struct ImpactEntryView {
    /// A stable category (`client`, `execution`, `plugin`, ...).
    pub kind: String,
    pub id: String,
    /// `None` is "unattributed": a reason to be more careful, not less.
    pub owner: Option<String>,
}

/// What a restart would disturb, plus the token that authorises it.
///
/// Exported to TypeScript as `RestartPreview` (the plan's name) rather than under
/// this module's `*View` convention: the desktop shell consumes it as a peer of the
/// lifecycle DTO it mirrors, and one name per concept is worth more than a uniform
/// suffix.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(
    export,
    export_to = "../../../../desktop/src/lib/generated/",
    rename = "RestartPreview"
)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeRestartPreviewView {
    /// Identifies this snapshot for paging. Not a credential by itself: every page
    /// request is still checked against the caller's grant and context.
    pub snapshot_id: String,
    /// The one-shot token a confirmation must present. Empty after the first page,
    /// which is what makes "paging cannot confirm" structural.
    pub token: String,
    pub revision: ImpactRevisionView,
    pub counts: ImpactCountsView,
    pub entries: Vec<ImpactEntryView>,
    pub next_cursor: Option<String>,
    /// Someone else, some running work, or an unknown impact: the UI must ask a
    /// second time.
    pub requires_second_confirmation: bool,
    /// Whether a restart may be attempted at all. `false` with a `blocked_reason`
    /// is an answer, not a failure.
    pub can_restart: bool,
    pub blocked_reason: Option<String>,
}

impl From<yi_agent_lifecycle::types::RestartPreview> for RuntimeRestartPreviewView {
    fn from(preview: yi_agent_lifecycle::types::RestartPreview) -> Self {
        Self {
            snapshot_id: preview.snapshot_id,
            token: preview.token,
            revision: ImpactRevisionView {
                guardian: preview.revision.guardian,
                coordinator: preview.revision.coordinator,
            },
            counts: ImpactCountsView::from(preview.counts),
            entries: preview
                .entries
                .into_iter()
                .map(|entry| ImpactEntryView {
                    kind: entry.kind,
                    id: entry.id,
                    owner: entry.owner,
                })
                .collect(),
            next_cursor: preview.next_cursor,
            requires_second_confirmation: preview.requires_second_confirmation,
            can_restart: preview.can_restart,
            blocked_reason: preview.blocked_reason,
        }
    }
}

/// Progress of one switch or reclaim operation.
///
/// Exported as `OperationStatus`, matching the lifecycle DTO it mirrors.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(
    export,
    export_to = "../../../../desktop/src/lib/generated/",
    rename = "OperationStatus"
)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeOperationStatusView {
    pub operation_id: String,
    pub phase: String,
    pub detail: Option<String>,
    /// The identity the new generation reported once it was verified, if any.
    pub ready_identity: Option<RuntimeIdentityView>,
}

/// The whole answer to `runtime/status`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
#[serde(rename_all = "camelCase")]
pub struct RuntimeStatusView {
    pub identity: RuntimeIdentityView,
    /// The build this *frontend* expects, when it knows.
    pub expected_build_id: Option<String>,
    /// The display version that goes with `expected_build_id`.
    pub expected_display_version: Option<String>,
    pub compatibility: Compatibility,
    /// Whether this runtime has the lifecycle machinery at all. A legacy runtime
    /// can be compatible for business and still have no exit guarantee, and a
    /// single `compatible: true` would erase that difference.
    pub lifecycle_managed: bool,
    pub counts: ImpactCountsView,
    pub clients: Vec<ClientSummaryView>,
    pub clients_next_cursor: Option<String>,
    pub active_operation: Option<RuntimeOperationStatusView>,
    /// Whether a restart could be started *from this machine*. Always `false` for a
    /// remote client: the control channel is a local socket, so a phone can display
    /// the mismatch but cannot act on it.
    pub local_restart_available: bool,
    /// Why the runtime is in the state it is, when the answer is not self-evident.
    pub message: Option<String>,
}

impl From<RuntimeIdentity> for RuntimeIdentityView {
    fn from(identity: RuntimeIdentity) -> Self {
        Self {
            scope_key: identity.scope_key,
            daemon_instance_id: identity.daemon_instance_id,
            guardian_build_id: identity.guardian_build_id,
            daemon_build_id: identity.daemon_build_id,
            mode: match identity.mode {
                yi_agent_lifecycle::types::RuntimeMode::Managed => RuntimeModeDto::Managed,
                yi_agent_lifecycle::types::RuntimeMode::LegacyIndependent => {
                    RuntimeModeDto::LegacyIndependent
                }
                yi_agent_lifecycle::types::RuntimeMode::LegacyEmbedded => {
                    RuntimeModeDto::LegacyEmbedded
                }
                yi_agent_lifecycle::types::RuntimeMode::Unknown => RuntimeModeDto::Unknown,
            },
            business_protocol: identity.business_protocol,
            daemon_display_version: identity.daemon_display_version,
            guardian_display_version: identity.guardian_display_version,
            capabilities: identity.capabilities,
            schema_min: identity.schema_min,
            schema_max: identity.schema_max,
        }
    }
}

impl From<ImpactCounts> for ImpactCountsView {
    fn from(counts: ImpactCounts) -> Self {
        Self {
            clients: counts.clients,
            pending_clients: counts.pending_clients,
            running: counts.running,
            queued: counts.queued,
            plugins: counts.plugins,
            complete: counts.complete,
        }
    }
}

impl From<OperationStatus> for RuntimeOperationStatusView {
    fn from(status: OperationStatus) -> Self {
        Self {
            operation_id: status.operation_id,
            // The phase travels as its snake-case wire name rather than a mirrored
            // enum: the switch phase list grows with the state machine, and a second
            // enum here would have to be updated in lockstep or silently mislabel a
            // phase.
            phase: serde_json::to_value(status.phase)
                .ok()
                .and_then(|value| value.as_str().map(str::to_string))
                .unwrap_or_else(|| "unknown".to_string()),
            detail: status.detail,
            ready_identity: status.ready_identity.map(RuntimeIdentityView::from),
        }
    }
}

/// What this app-server knows about the runtime it should describe.
///
/// `snapshot` is `None` when nothing answered: the absent case is a real answer
/// ("there is no managed runtime here"), so it is modelled rather than being an
/// error the caller has to interpret.
#[derive(Clone, Debug, Default)]
pub struct StatusSource {
    pub scope_key: String,
    pub expected: Option<ArtifactIdentity>,
    pub snapshot: Option<RuntimeStatusSnapshot>,
    /// The control channel could not be reached; carried so the view can explain
    /// *why* rather than just showing zeros.
    pub unreachable: Option<String>,
}

/// Builds the view from whatever the caller could observe.
///
/// Pure on purpose: every branch here is a decision a user can be shown, and a
/// decision that needs a socket to test is a decision nobody tests. The
/// classification delegates to the bring-up rule, so a runtime cannot be "same
/// build" here and "different build" there.
pub fn view(source: StatusSource, remote: bool) -> RuntimeStatusView {
    let expected_build_id = source
        .expected
        .as_ref()
        .map(|identity| identity.build_id.clone());
    let expected_display_version = source
        .expected
        .as_ref()
        .map(|identity| identity.display_version.clone());

    let Some(snapshot) = source.snapshot else {
        return RuntimeStatusView {
            identity: unknown_identity(&source.scope_key),
            expected_build_id,
            expected_display_version,
            // No answer is not "compatible": a socket that did not respond is the
            // definition of not knowing.
            compatibility: Compatibility::Unknown,
            lifecycle_managed: false,
            counts: ImpactCountsView::default(),
            clients: Vec::new(),
            clients_next_cursor: None,
            active_operation: None,
            local_restart_available: false,
            message: Some(
                source
                    .unreachable
                    .unwrap_or_else(|| "no runtime answered for this project".to_string()),
            ),
        };
    };

    let compatibility = classify(&snapshot.identity, source.expected.as_ref());
    let lifecycle_managed = matches!(
        snapshot.identity.mode,
        yi_agent_lifecycle::types::RuntimeMode::Managed
    );
    let message = message_for(compatibility, lifecycle_managed, &snapshot.identity);

    RuntimeStatusView {
        identity: RuntimeIdentityView::from(snapshot.identity),
        expected_build_id,
        expected_display_version,
        compatibility,
        lifecycle_managed,
        counts: ImpactCountsView::from(snapshot.counts),
        clients: snapshot
            .clients
            .into_iter()
            .map(|client| ClientSummaryView {
                client_run_id: client.client_run_id,
                kind: match client.kind {
                    ClientKind::App => ClientKindDto::App,
                    ClientKind::Tui => ClientKindDto::Tui,
                    ClientKind::Headless => ClientKindDto::Headless,
                },
                mode: match client.mode {
                    LeaseMode::Business => LeaseModeDto::Business,
                    LeaseMode::LeaseOnly => LeaseModeDto::LeaseOnly,
                    LeaseMode::WaitingNextGeneration => LeaseModeDto::WaitingNextGeneration,
                },
            })
            .collect(),
        clients_next_cursor: snapshot.clients_next_cursor,
        active_operation: snapshot
            .active_operation
            .map(RuntimeOperationStatusView::from),
        // A remote client can *see* this, but the control channel is a local unix
        // socket: offering it a restart button would be a promise the transport
        // cannot keep.
        local_restart_available: snapshot.local_restart_available && !remote,
        message,
    }
}

/// Classifies a running identity against what this frontend expected.
pub fn classify(identity: &RuntimeIdentity, expected: Option<&ArtifactIdentity>) -> Compatibility {
    let Some(expected) = expected else {
        // No expectation means this frontend has no opinion about the build. That is
        // `Unknown`, not "compatible": the banner must not claim equality it never
        // checked.
        return Compatibility::Unknown;
    };
    match yi_agent_supervisors::lifecycle::bringup::classify_compatibility(identity, expected, &[])
    {
        yi_agent_supervisors::lifecycle::bringup::Compatibility::SameBuild => {
            Compatibility::SameBuild
        }
        yi_agent_supervisors::lifecycle::bringup::Compatibility::CompatibleMismatch => {
            Compatibility::CompatibleMismatch
        }
        yi_agent_supervisors::lifecycle::bringup::Compatibility::ControlOnly => {
            Compatibility::ControlOnly
        }
        yi_agent_supervisors::lifecycle::bringup::Compatibility::LegacyTemporaryChoiceRequired => {
            Compatibility::LegacyTemporaryChoiceRequired
        }
        yi_agent_supervisors::lifecycle::bringup::Compatibility::Unknown => Compatibility::Unknown,
    }
}

fn message_for(
    compatibility: Compatibility,
    lifecycle_managed: bool,
    identity: &RuntimeIdentity,
) -> Option<String> {
    match compatibility {
        Compatibility::SameBuild => None,
        Compatibility::CompatibleMismatch => Some(
            match (
                identity.guardian_display_version.as_deref(),
                identity.daemon_display_version.as_deref(),
            ) {
                // Only claim "older" when there is comparable version evidence. Two
                // builds of one version are "different", not "newer" or "older".
                (Some(_), Some(_)) => {
                    "this project's runtime is a different build; new features may be unavailable"
                        .to_string()
                }
                _ => "this project's runtime is a different build".to_string(),
            },
        ),
        Compatibility::ControlOnly => Some(
            "the background runtime's business protocol is incompatible: tasks are disabled \
             until it is restarted"
                .to_string(),
        ),
        Compatibility::LegacyTemporaryChoiceRequired => Some(
            "this runtime was started by an older version and has no exit guarantees; \
             stop it by hand to get managed restarts"
                .to_string(),
        ),
        Compatibility::Unknown if !lifecycle_managed => {
            Some("the running runtime could not be identified".to_string())
        }
        Compatibility::Unknown => {
            Some("the running runtime's build could not be verified".to_string())
        }
    }
}

fn unknown_identity(scope_key: &str) -> RuntimeIdentityView {
    RuntimeIdentityView {
        scope_key: scope_key.to_string(),
        daemon_instance_id: None,
        guardian_build_id: None,
        daemon_build_id: None,
        mode: RuntimeModeDto::Unknown,
        business_protocol: None,
        daemon_display_version: None,
        guardian_display_version: None,
        capabilities: Vec::new(),
        schema_min: 0,
        schema_max: 0,
    }
}

/// Reads one status page from a guardian.
///
/// One request, no retry: a status read that retried would turn a wedged guardian
/// into a hang, and the caller polls again anyway. A missing socket is reported as
/// `Ok(None)` -- "no managed runtime here" is an answer, not a failure.
pub fn read_status(
    control_path: &Path,
    cursor: Option<&str>,
    budget: std::time::Duration,
) -> io::Result<Option<RuntimeStatusSnapshot>> {
    if !control_path.exists() {
        return Ok(None);
    }
    let deadline = Deadline::after(budget);
    let mut client =
        match yi_agent_lifecycle::client::ControlClient::connect(control_path, &deadline) {
            Ok(client) => client,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
    match client.request(
        &ControlRequest::Status {
            cursor: cursor.map(str::to_string),
        },
        &deadline,
    )? {
        ControlResponse::Status(snapshot) => Ok(Some(*snapshot)),
        // An older build that does not know this request: it is there, but it
        // cannot describe itself. Reported as "nothing to say" rather than as an
        // error, so the view shows `Unknown` with an explanation.
        ControlResponse::Unsupported { .. } => Ok(None),
        ControlResponse::Rejected { code, reason } => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("the runtime refused a status read ({code:?}): {reason}"),
        )),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("the runtime answered a status read with {other:?}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use yi_agent_lifecycle::types::RuntimeMode;

    fn expected(build: &str) -> ArtifactIdentity {
        ArtifactIdentity {
            build_id: build.to_string(),
            display_version: "0.2.0".to_string(),
            executable: PathBuf::from("/usr/bin/yi-agent"),
            sha256: "aa".to_string(),
            schema_min: 1,
            schema_max: 16,
            handover_version: 1,
            business_protocol_min: 1,
            business_protocol_max: 1,
            capabilities: vec!["lease.v1".to_string()],
        }
    }

    fn identity(build: &str, mode: RuntimeMode) -> RuntimeIdentity {
        RuntimeIdentity {
            scope_key: "/p/.yi-agent/runtime".to_string(),
            daemon_instance_id: Some("gen-1".to_string()),
            guardian_build_id: Some(build.to_string()),
            daemon_build_id: Some(build.to_string()),
            mode,
            business_protocol: Some(1),
            daemon_display_version: Some("0.2.0".to_string()),
            guardian_display_version: Some("0.2.0".to_string()),
            capabilities: vec!["lease.v1".to_string()],
            schema_min: 1,
            schema_max: 16,
        }
    }

    fn snapshot(identity: RuntimeIdentity) -> RuntimeStatusSnapshot {
        RuntimeStatusSnapshot {
            identity,
            counts: ImpactCounts {
                clients: 2,
                pending_clients: 0,
                running: 3,
                queued: 0,
                plugins: 1,
                complete: false,
            },
            clients: Vec::new(),
            clients_next_cursor: None,
            active_operation: None,
            local_restart_available: true,
        }
    }

    /// The matrix the UI actually branches on. Each row is a distinct user
    /// decision, so a single boolean would collapse information the banner needs.
    #[test]
    fn the_compatibility_matrix_is_classified_not_collapsed() {
        let cases = [
            (
                "same build, managed",
                identity("b1", RuntimeMode::Managed),
                Compatibility::SameBuild,
                true,
            ),
            (
                "different build, protocol compatible",
                identity("b2", RuntimeMode::Managed),
                Compatibility::CompatibleMismatch,
                true,
            ),
            (
                "legacy runtime",
                identity("b1", RuntimeMode::LegacyIndependent),
                Compatibility::LegacyTemporaryChoiceRequired,
                false,
            ),
            (
                "unknown mode",
                identity("b1", RuntimeMode::Unknown),
                Compatibility::Unknown,
                false,
            ),
        ];
        for (name, running, want, managed) in cases {
            let view = view(
                StatusSource {
                    scope_key: "/p/.ya/runtime".to_string(),
                    expected: Some(expected("b1")),
                    snapshot: Some(snapshot(running)),
                    unreachable: None,
                },
                false,
            );
            assert_eq!(view.compatibility, want, "{name}");
            assert_eq!(view.lifecycle_managed, managed, "{name}");
        }
    }

    /// A business-incompatible runtime with a working control channel is
    /// `ControlOnly`, not `Unknown`: it can be switched, it just cannot run tasks.
    ///
    /// The `control.v1` capability is what licenses that answer. Without it the
    /// same protocol mismatch is `Unknown` -- see
    /// `a_protocol_mismatch_without_control_capability_is_unknown`.
    #[test]
    fn a_business_incompatible_but_controllable_runtime_is_control_only() {
        let mut running = identity("b2", RuntimeMode::Managed);
        running.business_protocol = Some(99);
        running.capabilities = vec!["lease.v1".to_string(), "control.v1".to_string()];
        let view = view(
            StatusSource {
                scope_key: "/p/.ya/runtime".to_string(),
                expected: Some(expected("b1")),
                snapshot: Some(snapshot(running)),
                unreachable: None,
            },
            false,
        );
        assert_eq!(view.compatibility, Compatibility::ControlOnly);
        assert!(view.lifecycle_managed);
    }

    /// A protocol mismatch is not automatically "control still works": that claim
    /// needs the `control.v1` capability, and without it the honest answer is
    /// `Unknown`. This is the case that stops a UI from offering an upgrade button
    /// to a runtime that never implemented the control channel.
    #[test]
    fn a_protocol_mismatch_without_control_capability_is_unknown() {
        let mut running = identity("b2", RuntimeMode::Managed);
        running.business_protocol = Some(99);
        let view = view(
            StatusSource {
                scope_key: "/p/.ya/runtime".to_string(),
                expected: Some(expected("b1")),
                snapshot: Some(snapshot(running)),
                unreachable: None,
            },
            false,
        );
        assert_eq!(view.compatibility, Compatibility::Unknown);
    }

    /// An error frame from a business RPC is not evidence of a healthy runtime.
    /// With nothing answering, the answer is `Unknown` and the reason is surfaced.
    #[test]
    fn no_answer_is_unknown_and_explains_itself() {
        let view = view(
            StatusSource {
                scope_key: "/p/.ya/runtime".to_string(),
                expected: Some(expected("b1")),
                snapshot: None,
                unreachable: Some("the runtime socket did not answer".to_string()),
            },
            false,
        );
        assert_eq!(view.compatibility, Compatibility::Unknown);
        assert!(!view.lifecycle_managed);
        assert_eq!(
            view.message.as_deref(),
            Some("the runtime socket did not answer")
        );
        assert_eq!(view.counts.running, 0);
        assert!(!view.counts.complete);
    }

    /// No expectation is not the same as matching. A frontend that cannot say what
    /// it expects must not be told the runtime agrees with it.
    #[test]
    fn a_frontend_without_an_expectation_never_claims_a_match() {
        let compatibility = classify(&identity("b1", RuntimeMode::Managed), None);
        assert_eq!(compatibility, Compatibility::Unknown);
    }

    /// A remote client may see the runtime but must not be offered a control that
    /// only exists as a local socket.
    #[test]
    fn a_remote_client_never_sees_local_restart_available() {
        let source = || StatusSource {
            scope_key: "/p/.ya/runtime".to_string(),
            expected: Some(expected("b1")),
            snapshot: Some(snapshot(identity("b1", RuntimeMode::Managed))),
            unreachable: None,
        };
        assert!(view(source(), false).local_restart_available);
        assert!(!view(source(), true).local_restart_available);
    }

    /// The read-only set is closed: a destructive sibling that a future author adds
    /// to `runtime/*` is not remote-callable by accident.
    #[test]
    fn the_read_only_set_does_not_grow_by_prefix() {
        assert!(is_read_only_runtime_method("runtime/status"));
        assert!(!is_read_only_runtime_method("runtime/confirmRestart"));
        assert!(!is_read_only_runtime_method("runtime/stop"));
        assert!(!is_read_only_runtime_method("runtime/status/extra"));
    }

    /// Every field of the lifecycle snapshot survives the trip into the view. A
    /// silently dropped field is the failure mode a roundtrip test catches.
    #[test]
    fn the_view_keeps_every_field_of_the_snapshot() {
        let mut running = snapshot(identity("b1", RuntimeMode::Managed));
        running.counts = ImpactCounts {
            clients: 4,
            pending_clients: 1,
            running: 2,
            queued: 7,
            plugins: 3,
            complete: true,
        };
        running.clients = vec![yi_agent_lifecycle::types::ClientSummary {
            client_run_id: "tui-1".to_string(),
            kind: ClientKind::Tui,
            mode: LeaseMode::LeaseOnly,
        }];
        running.clients_next_cursor = Some("tui-1".to_string());
        running.active_operation = Some(OperationStatus {
            operation_id: "op-1".to_string(),
            phase: yi_agent_lifecycle::types::SwitchPhase::Draining,
            detail: Some("waiting for clients".to_string()),
            ready_identity: None,
        });

        let view = view(
            StatusSource {
                scope_key: "/p/.ya/runtime".to_string(),
                expected: Some(expected("b1")),
                snapshot: Some(running),
                unreachable: None,
            },
            false,
        );

        assert_eq!(view.counts.clients, 4);
        assert_eq!(view.counts.pending_clients, 1);
        assert_eq!(view.counts.running, 2);
        assert_eq!(view.counts.queued, 7);
        assert_eq!(view.counts.plugins, 3);
        assert!(view.counts.complete);
        assert_eq!(view.clients.len(), 1);
        assert_eq!(view.clients[0].client_run_id, "tui-1");
        assert_eq!(view.clients[0].kind, ClientKindDto::Tui);
        assert_eq!(view.clients[0].mode, LeaseModeDto::LeaseOnly);
        assert_eq!(view.clients_next_cursor.as_deref(), Some("tui-1"));
        let operation = view.active_operation.expect("the operation must survive");
        assert_eq!(operation.operation_id, "op-1");
        assert_eq!(operation.phase, "draining");
        assert_eq!(operation.detail.as_deref(), Some("waiting for clients"));
        assert_eq!(view.expected_build_id.as_deref(), Some("b1"));
        assert_eq!(view.expected_display_version.as_deref(), Some("0.2.0"));
    }

    /// Every field of a preview survives into the view.
    ///
    /// A preview is what the user approves, so a dropped field is not cosmetic: a
    /// lost `requiresSecondConfirmation` turns a two-step confirmation into one, and
    /// a lost `coordinator` revision turns "unknown impact" into "no impact".
    #[test]
    fn the_preview_view_keeps_every_field() {
        let preview = yi_agent_lifecycle::types::RestartPreview {
            snapshot_id: "snap-1".to_string(),
            token: "tok-1".to_string(),
            revision: yi_agent_lifecycle::types::ImpactRevision {
                guardian: 7,
                coordinator: None,
            },
            counts: ImpactCounts {
                clients: 2,
                pending_clients: 1,
                running: 3,
                queued: 4,
                plugins: 5,
                complete: false,
            },
            entries: vec![yi_agent_lifecycle::types::ImpactEntry {
                kind: "execution".to_string(),
                id: "run-1".to_string(),
                owner: None,
            }],
            next_cursor: Some("run-1".to_string()),
            requires_second_confirmation: true,
            can_restart: false,
            blocked_reason: Some("a hand-started runtime cannot be replaced".to_string()),
        };
        let view = RuntimeRestartPreviewView::from(preview);
        let json = serde_json::to_value(&view).unwrap();
        let back: RuntimeRestartPreviewView = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(back, view);

        assert_eq!(json["snapshotId"], "snap-1");
        assert_eq!(json["token"], "tok-1");
        assert_eq!(json["revision"]["guardian"], 7);
        // `None` must survive as an explicit null, not disappear: absent would read
        // as "zero", which is the one thing an unknown impact is not.
        assert!(json["revision"].get("coordinator").is_some());
        assert!(json["revision"]["coordinator"].is_null());
        assert_eq!(json["counts"]["pendingClients"], 1);
        assert_eq!(json["counts"]["complete"], false);
        assert_eq!(json["entries"][0]["kind"], "execution");
        assert!(json["entries"][0]["owner"].is_null());
        assert_eq!(json["nextCursor"], "run-1");
        assert_eq!(json["requiresSecondConfirmation"], true);
        assert_eq!(json["canRestart"], false);
        assert_eq!(
            json["blockedReason"],
            "a hand-started runtime cannot be replaced"
        );
    }

    /// The view must survive a JSON round trip: it is a wire DTO, and a field that
    /// only exists in Rust is a field the TypeScript consumer never sees.
    #[test]
    fn the_view_round_trips_through_json() {
        let view = view(
            StatusSource {
                scope_key: "/p/.ya/runtime".to_string(),
                expected: Some(expected("b1")),
                snapshot: Some(snapshot(identity("b1", RuntimeMode::Managed))),
                unreachable: None,
            },
            false,
        );
        let json = serde_json::to_value(&view).unwrap();
        assert!(json.get("clientsNextCursor").is_some(), "{json}");
        assert!(json.get("localRestartAvailable").is_some(), "{json}");
        assert!(json.get("lifecycleManaged").is_some(), "{json}");
        let back: RuntimeStatusView = serde_json::from_value(json).unwrap();
        assert_eq!(back, view);
    }
}
