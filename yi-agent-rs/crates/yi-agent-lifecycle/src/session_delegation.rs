//! Wire contract for delegating from a *session container*.
//!
//! Two axes are deliberately separate here:
//!
//! * Identity — may this host act for this session at all? Managed hosts prove it
//!   through the guardian-issued `RootAuthorization`; embedded hosts prove it with
//!   a private resume secret that never leaves the host's own storage.
//! * Admission — may *this turn* write? That is a short-lived fence signed by the
//!   daemon for one binding generation and handed to one run's tool snapshot.
//!
//! A historical `TaskState` appears in neither axis. That is the whole point: a
//! session whose last execution stalled must still be able to delegate, while a
//! settled execution must not.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::foreground::{QuiescenceRef, SecretCapability};
use crate::types::RootAuthorization;

/// The capability a daemon advertises only after every route is fenced.
pub const SESSION_DELEGATION_CAPABILITY: &str = "session_delegation_lifecycle_v1";

/// Domain separation for the resume digest.
///
/// The digest is persisted, so it must not be confusable with a digest computed
/// for any other purpose over the same secret.
const RESUME_DIGEST_DOMAIN: &[u8] = b"session-resume-v1\0";

/// A stable session address: the public thread key and the root task.
///
/// Deliberately no attempt id. `RootRef.attempt_id` remains a compatibility
/// address for execution history; a container never reopens an attempt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRef {
    pub session_id: String,
    pub root_id: String,
}

/// Which daemon generation and binding may act.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFence {
    pub root: SessionRef,
    pub generation: String,
    pub binding_epoch: u64,
    pub driver_epoch: u64,
}

/// One open turn plus the only capability a tool may hold.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTurn {
    pub fence: SessionFence,
    pub turn_id: String,
    pub capability: SecretCapability,
}

/// A host's binding for one daemon generation.
///
/// This capability opens host-private control channels; it is never handed to a
/// tool, which only ever sees the narrower [`SessionTurn`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionBinding {
    pub fence: SessionFence,
    pub capability: SecretCapability,
}

/// How a host proves it may act for a session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionIdentity {
    /// Guardian-authorized: the existing managed path, unchanged.
    Managed(RootAuthorization),
    /// A private resume secret the host stored itself.
    Embedded(SecretCapability),
    /// A legacy application capability, consumed at most once.
    LegacyUpgrade(SecretCapability),
}

/// Ask the daemon to (re)establish a binding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrepareSession {
    pub root: SessionRef,
    pub operation_id: String,
    /// Random per host process; proves which live owner is asking. Not authority.
    pub host_run_id: String,
    pub expected_binding_epoch: u64,
    pub identity: SessionIdentity,
    /// The caller's current binding, if it still has one. A live binding owned by
    /// this host is reused instead of bumping the epoch every turn; a bare
    /// `host_run_id` is not enough to get the secret back.
    pub current_binding: Option<SessionBinding>,
    /// New resume digest, written only when the identity is upgraded/replaced.
    pub replacement_resume_digest: Option<String>,
    /// References to durable stop evidence. The wire cannot create evidence.
    pub stopped: Vec<QuiescenceRef>,
}

/// Stable failure kinds, so callers branch on codes instead of prose.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionFailureCode {
    SessionIdentityRequired,
    AuthorityDenied,
    SessionClosed,
    OwnerConflict,
    StaleGeneration,
    StaleBinding,
    StaleTurn,
    StopUnproven,
    AdmissionClosed,
    ProtocolMismatch,
    CorruptAttachment,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFailure {
    pub code: SessionFailureCode,
    pub reason: String,
}

impl SessionFailure {
    pub fn new(code: SessionFailureCode, reason: impl Into<String>) -> Self {
        Self {
            code,
            reason: reason.into(),
        }
    }
}

/// SHA-256 of a resume secret, domain-separated and lower-case hex.
///
/// Shared by the host (which stores the secret) and the store (which only keeps
/// this digest), so neither side re-implements the hash.
pub fn session_resume_digest(secret: &SecretCapability) -> String {
    let mut hasher = Sha256::new();
    hasher.update(RESUME_DIGEST_DOMAIN);
    hasher.update(secret.expose().as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_delegation_lifecycle_secret_debug_is_redacted() {
        let turn = SessionTurn {
            fence: SessionFence {
                root: SessionRef {
                    session_id: "s".into(),
                    root_id: "r".into(),
                },
                generation: "g".into(),
                binding_epoch: 1,
                driver_epoch: 1,
            },
            turn_id: "t".into(),
            capability: SecretCapability::new("never-log-me"),
        };
        let debug = format!("{turn:?}");
        assert!(!debug.contains("never-log-me"), "{debug}");
        let json = serde_json::to_string(&turn).unwrap();
        let back: SessionTurn = serde_json::from_str(&json).unwrap();
        assert_eq!(back, turn);
    }

    #[test]
    fn session_delegation_lifecycle_resume_digest_is_domain_separated() {
        let secret = SecretCapability::new("abc");
        let digest = session_resume_digest(&secret);
        assert_eq!(digest.len(), 64);
        assert!(
            digest
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_eq!(digest, session_resume_digest(&SecretCapability::new("abc")));
        assert_ne!(digest, session_resume_digest(&SecretCapability::new("abd")));
    }
}
