//! Cross-process DTOs for the foreground root lifecycle.
//!
//! A foreground application root is the container a long-lived user conversation
//! runs in. It is **not** a background worker attempt, so it must not be settled by
//! the worker watchdog ([`crate::types`] stays the home of lease/grant secrets).
//! Everything here is wire data: it crosses from the guardian to the daemon, from
//! the daemon back to the host, and from the host to the UI.
//!
//! Two rules bind every type in this module:
//!
//! * Secrets get a `Debug` that prints `<redacted>`. [`SecretCapability`] is the
//!   only home for a driver/confirmation capability, and it never implements
//!   `Display`, so an accidental `{}` in a format string cannot leak it.
//! * Anything compared for admission derives `PartialEq`/`Eq`; nothing stores a
//!   float, because the fence values are compared for equality, not for range.

use serde::{Deserialize, Serialize};

/// The wire capability name for the whole closed-loop feature.
///
/// It is advertised only once every entry point is wired; an intermediate build
/// that defines the types must not claim it.
pub const FOREGROUND_ROOT_CAPABILITY: &str = "foreground_root_lifecycle_v1";

/// The three ids that address one foreground attempt.
///
/// Strings on the wire (the daemon turns them into its strong `TaskId` /
/// `AttemptId` / `RootSessionId` types at the business entry point); a parse
/// failure there is a validation error, never a silent reinterpretation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootRef {
    pub session_id: String,
    pub root_id: String,
    pub attempt_id: String,
}

/// The full admission fence of one attempt of one root in one daemon generation.
///
/// Every write carries it so an update generated for an older attempt, an older
/// driver or an older daemon cannot land on a newer one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForegroundFence {
    pub root: RootRef,
    pub generation: String,
    pub binding_epoch: u64,
    pub driver_epoch: u64,
}

/// What a foreground conversation is doing, independent of `TaskState`.
///
/// `Unknown` is produced by the daemon when the driver control connection dies or
/// the daemon restarts. It is *not* a legal inbound value from a driver: a driver
/// that cannot tell what it is doing has no business reporting a phase at all, and
/// only a proven driver replacement may reset the snapshot (see the store's
/// `reduce_activity`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityPhase {
    Idle,
    Running,
    WaitingForChildren,
    WaitingForPermission,
    Unknown,
}

/// One ordered activity report.
///
/// `seq` is assigned by the driver and increases within one `driver_epoch`; it is
/// the ordering key that makes retries idempotent and stale reports rejectable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityUpdate {
    pub fence: ForegroundFence,
    pub turn_id: Option<String>,
    pub seq: u64,
    pub phase: ActivityPhase,
}

/// The committed activity view plus when it changed.
///
/// `changed_at` is an RFC3339 observation timestamp. It is deliberately *not* part
/// of any liveness decision: replaying a snapshot never extends authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivitySnapshot {
    pub update: ActivityUpdate,
    pub changed_at: String,
}

/// What the daemon hands a driver once it accepts the binding.
///
/// `driver_capability` is the narrow secret that authorises *this* driver epoch to
/// write activity for *this* root. It is never shown to the UI and never travels
/// through the app-server to a model.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriverBinding {
    pub fence: ForegroundFence,
    pub last_seq: u64,
    pub driver_capability: SecretCapability,
}

/// What the host asks the guardian for when a conversation needs a root.
///
/// `thread_key` is the stable conversation key; `previous_root_capability` is only
/// present when a host is asking to recover a conversation whose old owner is
/// already gone. It is never derivable from the thread key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootBindIntent {
    pub operation_id: String,
    pub thread_key: String,
    pub workspace: std::path::PathBuf,
    pub extra_roots: Vec<std::path::PathBuf>,
    pub previous_root_capability: Option<SecretCapability>,
}

/// How much authority one installed root authorization carries.
///
/// `RecoveryOnly` exists to break the "you must stop it before you are allowed to
/// authorise it, and you must be authorised before you may stop it" cycle: it can
/// inspect, freeze, stop, prepare and query, but it can never bind a driver, spawn,
/// write activity, or start a tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootAuthorityScope {
    Business,
    RecoveryOnly,
}

/// The admission facts the guardian installs alongside a root authorization.
///
/// `challenge_nonce` and `remaining_lease_ms` are the guard against a queued
/// message outliving the lease it proves: the daemon records `t0` when it emits the
/// challenge and computes the conservative deadline as `t0 + remaining_lease_ms`,
/// never "receipt time + the full window".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorityStamp {
    pub scope_key: String,
    pub generation: String,
    pub owner: String,
    pub root_id: String,
    pub binding_epoch: u64,
    pub authority_seq: u64,
    pub challenge_nonce: String,
    pub remaining_lease_ms: u64,
    pub authority_scope: RootAuthorityScope,
}

/// One ordered message on the private guardian -> daemon authority bridge.
///
/// This is not a business IPC variant: the daemon accepts it only from the inherited
/// descriptor created at bring-up, never from the public socket, so a tool holding a
/// managed-spawn channel cannot install authority for itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthorityMessage {
    Install {
        stamp: AuthorityStamp,
        authorization: crate::types::RootAuthorization,
    },
    Renew {
        stamp: AuthorityStamp,
    },
    RevokeOwner {
        owner: String,
        generation: String,
        authority_seq: u64,
    },
    Drain {
        generation: String,
        authority_seq: u64,
    },
}

/// One frame on the private guardian <-> daemon authority bridge.
///
/// The bridge is bidirectional, and that is not an accident of convenience: the
/// deadline bound only works if the *daemon* decides when the clock started. So the
/// daemon asks first ([`Self::Challenge`]), records the instant locally, and only then
/// accepts an [`AuthorityMessage`] that answers it.
///
/// The guardian must never send an `Authority` frame it was not asked for, and the
/// daemon must never accept one that no challenge covers. A one-way "install" frame
/// would let a replayed message outlive the lease that justified it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BridgeFrame {
    /// Guardian -> daemon: "the host wants authority for this root under this
    /// operation".
    ///
    /// Nothing is installed by this frame. It only lets the daemon reserve the root,
    /// check what it already knows, and *then* issue the [`Self::Challenge`] whose
    /// issue instant starts the deadline clock. A guardian that instead pushed an
    /// `Authority` frame first would be choosing its own deadline.
    Prepare {
        nonce: String,
        root_id: String,
        client_run_id: String,
        scope_key: String,
        generation: String,
        /// The lowest sequence the daemon will accept an answer at. Derived from its
        /// tombstones, so a revocation that already happened cannot be undone by a
        /// message the guardian had queued.
        requested_seq: u64,
        intent: RootBindIntent,
        /// Whether the request is for business authority or for the narrower
        /// recovery-only scope.
        scope: RootAuthorityScope,
    },
    /// Daemon -> guardian: "validate authority for this root under this nonce".
    ///
    /// `authority_seq` tells the guardian the lowest sequence it may use; a guardian
    /// that answers with an older one is refused, which is what keeps a
    /// post-revocation sequence from being reused.
    Challenge {
        nonce: String,
        root_id: String,
        authority_seq: u64,
    },
    /// Daemon -> guardian: the install was validated and committed in the same step.
    ///
    /// This is the only frame that answers "is this root now managed". A guardian that
    /// treated its own `Authority` send as success would report a binding the daemon
    /// had refused.
    Committed {
        nonce: String,
        root_id: String,
        binding_epoch: u64,
    },
    /// Daemon -> guardian: the prepare could not proceed, with a wire-safe reason.
    Denied {
        nonce: String,
        root_id: String,
        code: ForegroundErrorCode,
    },
    /// Guardian -> daemon: the ordered authority message answering a challenge.
    Authority(AuthorityMessage),
    /// Guardian -> daemon: what it could not do, so the wait ends explicitly.
    ///
    /// Without this the daemon's prepare would have to time out to learn that the
    /// guardian refused, and a refusal would be indistinguishable from a wedged
    /// guardian.
    Refused {
        nonce: String,
        reason: AuthorityRefusal,
    },
}

/// Why the guardian declined to answer a challenge.
///
/// Whitelisted phrases only: this crosses a process boundary into a log line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorityRefusal {
    /// The guardian serves a different generation or scope than the daemon asks about.
    WrongGeneration,
    /// The asking root has no live owner the guardian knows of.
    NoOwner,
    /// The guardian could not reach its lease table in time; retry is meaningful.
    Unavailable,
}

/// One explicit recovery request for a root whose attempt was settled by mistake.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryRequest {
    pub expected: ForegroundFence,
    pub operation_id: String,
}

/// What a freeze would stop, and what it could not prove stopped.
///
/// `blocked_execution_ids` is the reason a confirmation token is withheld: a
/// recovery that cannot show every accepted execution stopped must not be
/// confirmable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryImpact {
    pub root: RootRef,
    pub stopped_execution_ids: Vec<String>,
    pub blocked_execution_ids: Vec<String>,
    pub child_ids: Vec<String>,
    pub version: u64,
    pub reason: String,
}

/// The result of preparing a recovery: what it would do, and whether it may proceed.
///
/// `confirmation_token` is `None` while stops are unproven. `expires_at` is only
/// meaningful when a token was issued; a blocked or in-progress preparation must not
/// invent an expiry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryPreview {
    pub request: RecoveryRequest,
    pub impact: RecoveryImpact,
    pub confirmation_token: Option<SecretCapability>,
    pub expires_at: Option<String>,
}

/// The durable identity of a successful recovery.
///
/// It contains no capability: a host that recovers a root must bind a driver again
/// to obtain the current narrow capability, so an idempotent replay can never hand
/// out authority twice.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryResult {
    pub operation_id: String,
    pub previous: RootRef,
    pub successor: RootRef,
    pub binding_epoch: u64,
    pub driver_epoch: u64,
}

// ---------------------------------------------------------------------------
// Structured failures (shared by IPC, the coordinator and the host)
// ---------------------------------------------------------------------------

/// Machine-readable failure reasons for every foreground operation.
///
/// Deliberately payload-free and compared by value: dispatch on these, never on an
/// English error string.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForegroundErrorCode {
    Unsupported,
    AuthorityDenied,
    AdmissionClosed,
    StaleAttempt,
    StaleDriver,
    InvalidActivity,
    SequenceConflict,
    RecoveryRequired,
    RecoveryAuthorityMissing,
    StopUnproven,
    ConfirmationRequired,
    ConfirmationExpired,
    OperationConflict,
    CorruptBinding,
}

/// A structured foreground rejection on the wire.
///
/// `StopUnproven` carries its blocking entities here rather than in the code, so the
/// code enum stays a pure discriminant. `reason` is a short whitelisted phrase, never
/// a formatted upstream error: error text has leaked paths and secrets before.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForegroundFailure {
    pub code: ForegroundErrorCode,
    pub blocked_execution_ids: Vec<String>,
    pub reason: String,
}

impl ForegroundFailure {
    /// Builds a failure with no blocking entities.
    pub fn new(code: ForegroundErrorCode, reason: impl Into<String>) -> Self {
        Self {
            code,
            blocked_execution_ids: Vec::new(),
            reason: reason.into(),
        }
    }
}

/// The observable state of one recovery operation, including the non-success ones.
///
/// `Preparing`, `Blocked` and `NeedsPreview` are explicitly *not* success. Only
/// `Committed` means a successor attempt exists.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryOperationStatus {
    Preparing,
    /// After a restart the freeze and history survive but the in-memory token does
    /// not, so the host must prepare again under valid authorization.
    NeedsPreview,
    Blocked(RecoveryImpact),
    Ready(RecoveryPreview),
    Committed(RecoveryResult),
}

impl RecoveryOperationStatus {
    /// The stable wire tag for this status.
    ///
    /// The IPC codec and the host/UI both switch on this, so pinning it next to the
    /// enum keeps the serde representation and the explicit dispatch in one place.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Preparing => "preparing",
            Self::NeedsPreview => "needs_preview",
            Self::Blocked(_) => "blocked",
            Self::Ready(_) => "ready",
            Self::Committed(_) => "committed",
        }
    }
}

// ---------------------------------------------------------------------------
// Redacting secret
// ---------------------------------------------------------------------------

/// A capability that must never be printed.
///
/// There is no `Display` impl on purpose: `tracing`'s `{}` and `format!` both need
/// one, so their absence is a compile error rather than a leak. `Debug` exists
/// because the containing DTOs derive it, and prints `<redacted>`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretCapability(String);

impl SecretCapability {
    pub fn new(secret: impl Into<String>) -> Self {
        Self(secret.into())
    }

    /// The only way to read the secret. Call sites should be few and auditable.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretCapability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreground_root_lifecycle_secret_capability_debug_never_contains_the_secret() {
        let secret = SecretCapability::new("cap-super-secret-value");
        let rendered = format!("{secret:?}");
        assert_eq!(rendered, "<redacted>");
        assert!(!rendered.contains("super-secret"));
        assert_eq!(secret.expose(), "cap-super-secret-value");
    }

    #[test]
    fn foreground_root_lifecycle_nested_dto_debug_redacts_the_driver_capability() {
        let binding = DriverBinding {
            fence: ForegroundFence {
                root: RootRef {
                    session_id: "session-1".into(),
                    root_id: "root-1".into(),
                    attempt_id: "attempt-1".into(),
                },
                generation: "gen-1".into(),
                binding_epoch: 1,
                driver_epoch: 1,
            },
            last_seq: 3,
            driver_capability: SecretCapability::new("driver-secret-xyz"),
        };
        let rendered = format!("{binding:?}");
        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains("driver-secret-xyz"));
        // Non-secret diagnostics must survive, otherwise the Debug is useless.
        assert!(rendered.contains("gen-1"));
    }

    #[test]
    fn foreground_root_lifecycle_secret_capability_round_trips_through_json() {
        let secret = SecretCapability::new("cap-1");
        let json = serde_json::to_string(&secret).unwrap();
        // Transparent: the wire form is the bare string, not a tagged wrapper.
        assert_eq!(json, "\"cap-1\"");
        let decoded: SecretCapability = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, secret);
    }

    #[test]
    fn foreground_root_lifecycle_recovery_preview_withholds_the_token_when_blocked() {
        let impact = RecoveryImpact {
            root: RootRef {
                session_id: "session-1".into(),
                root_id: "root-1".into(),
                attempt_id: "attempt-1".into(),
            },
            stopped_execution_ids: vec![],
            blocked_execution_ids: vec!["exec-2".into()],
            child_ids: vec!["child-1".into()],
            version: 1,
            reason: "stalled".into(),
        };
        let preview = RecoveryPreview {
            request: RecoveryRequest {
                expected: ForegroundFence {
                    root: impact.root.clone(),
                    generation: "gen-1".into(),
                    binding_epoch: 1,
                    driver_epoch: 1,
                },
                operation_id: "recover-1".into(),
            },
            impact: impact.clone(),
            confirmation_token: None,
            expires_at: None,
        };
        // A blocked preparation cannot be confirmed: no token, no invented expiry.
        assert!(preview.confirmation_token.is_none());
        assert!(preview.expires_at.is_none());
        assert_eq!(RecoveryOperationStatus::Blocked(impact).code(), "blocked");
    }

    #[test]
    fn foreground_root_lifecycle_operation_status_encodes_non_success_states() {
        let statuses = [
            (RecoveryOperationStatus::Preparing, "preparing"),
            (RecoveryOperationStatus::NeedsPreview, "needs_preview"),
        ];
        for (status, expected) in statuses {
            assert_eq!(status.code(), expected);
        }
    }

    #[test]
    fn foreground_root_lifecycle_failure_carries_blocking_entities_out_of_band() {
        let failure = ForegroundFailure {
            code: ForegroundErrorCode::StopUnproven,
            blocked_execution_ids: vec!["exec-1".into()],
            reason: "the old execution is not proven stopped".into(),
        };
        let json = serde_json::to_value(&failure).unwrap();
        assert_eq!(json["code"], "stop_unproven");
        assert_eq!(json["blocked_execution_ids"][0], "exec-1");
        let decoded: ForegroundFailure = serde_json::from_value(json).unwrap();
        assert_eq!(decoded, failure);
    }

    #[test]
    fn foreground_root_lifecycle_capability_constant_is_stable() {
        assert_eq!(FOREGROUND_ROOT_CAPABILITY, "foreground_root_lifecycle_v1");
    }
}
