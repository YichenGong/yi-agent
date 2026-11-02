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
