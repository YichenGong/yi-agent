//! Per-turn session delegation for the app-server.
//!
//! The app-server used to treat delegation as a once-per-process activation: it
//! attached a root, got a capability, and handed that same capability to every
//! turn's tools. Two consequences were both wrong:
//!
//! * A tool from a finished turn kept a capability that could still write, so a
//!   slow call could spawn after its own turn ended.
//! * "The conversation may delegate" was decided once, so a conversation whose
//!   first activation failed stayed broken until the app restarted.
//!
//! This module makes delegation a **per-turn** fact. Each turn prepares the
//! container binding, opens a turn, installs the turn's own tool set through
//! [`yi_agent_core::Agent::set_tools_for_next_run`], and closes the turn on every
//! exit path. The binding is reused across turns; only the turn and the secrets
//! change.
//!
//! Failure is reported, never silently swallowed: a turn whose delegation could
//! not be prepared still runs (the conversation must stay usable), but the client
//! is told through `thread/delegationStatus/updated` so "I could not spawn" is a
//! visible state rather than a mystery.

use std::path::Path;
use std::sync::Arc;

use yi_agent_lifecycle::foreground::{
    EvidenceSource, ExecutionIdentity, ForegroundFence, RootRef, SecretCapability, StopEvidence,
};
use yi_agent_lifecycle::session_delegation::{
    SessionFailure, SessionIdentity, session_resume_digest,
};
use yi_agent_subagent::session_delegation::{SessionClientError, SessionHost, SessionTurnClient};
use yi_agent_subagent::session_identity::SessionIdentityStore;
use yi_agent_subagent::thread_root::ForegroundAuthority;
use yi_agent_subagent::thread_root::ThreadRoot;

/// Why a turn could not be prepared for delegation.
///
/// Only the first variant says something about the conversation; the second says
/// this build/daemon pair cannot offer the contract at all. Keeping them apart
/// matters because the first is worth retrying next turn and the second is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DelegationFailure {
    /// The daemon refused, or the identity did not check out.
    Refused(SessionFailure),
    /// No usable runtime, or the two sides disagree about the protocol.
    Unavailable(String),
}

impl DelegationFailure {
    /// The wire shape the client branches on: a stable code plus a human reason.
    pub(crate) fn payload(&self) -> crate::protocol::DelegationFailurePayload {
        match self {
            Self::Refused(failure) => crate::protocol::DelegationFailurePayload {
                // Serialized, not formatted: the wire code is the `snake_case`
                // name the daemon already uses, so a client can branch on it
                // without re-deriving a mapping from Rust identifiers.
                code: serde_json::to_value(failure.code)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_string))
                    .unwrap_or_else(|| "protocol_mismatch".to_string()),
                reason: failure.reason.clone(),
            },
            Self::Unavailable(reason) => crate::protocol::DelegationFailurePayload {
                code: "unavailable".to_string(),
                reason: reason.clone(),
            },
        }
    }

    fn from_client(error: SessionClientError) -> Self {
        match error {
            SessionClientError::Rejected(failure) => Self::Refused(failure),
            SessionClientError::Transport(message) => Self::Unavailable(message),
            SessionClientError::Protocol(message) => Self::Unavailable(message),
        }
    }
}

/// What a turn's delegation preparation produced.
///
/// `Ready` carries the turn client the tool set must hold. The other two carry a
/// failure *and* no capability: a turn that could not be prepared must not be
/// given a tool set that looks authorised.
pub(crate) enum PreparedTurn {
    /// The turn is open and its execution is registered, so the daemon will hold
    /// this turn's write authority until it is told the execution stopped.
    Ready {
        turn: Arc<SessionTurnClient>,
        execution: ExecutionIdentity,
    },
    NotReady(DelegationFailure),
}

impl PreparedTurn {
    /// Only the tests read the failure through this accessor; the driver matches
    /// on the variant, so an unused-in-production method would be dead code.
    #[cfg(test)]
    pub(crate) fn failure(&self) -> Option<&DelegationFailure> {
        match self {
            Self::Ready { .. } => None,
            Self::NotReady(failure) => Some(failure),
        }
    }
}

impl std::fmt::Debug for PreparedTurn {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ready { .. } => formatter.write_str("PreparedTurn::Ready"),
            Self::NotReady(failure) => formatter
                .debug_tuple("PreparedTurn::NotReady")
                .field(failure)
                .finish(),
        }
    }
}

/// The app-server's delegation driver for one thread.
///
/// Holds the host (which owns the binding secret) for as long as the thread lives.
/// The host is what makes a restart recoverable: the resume secret is on disk, so
/// a new process can prove it is the same conversation instead of creating a
/// second container.
pub(crate) struct ThreadDelegation {
    root: Arc<ThreadRoot>,
    identities: SessionIdentityStore,
    /// `None` until the first turn that had a usable runtime prepared it.
    host: Option<SessionHost>,
    /// The resume secret for this conversation, loaded once.
    secret: Option<SecretCapability>,
    /// Set when a turn closed locally but the daemon never confirmed it. Until it
    /// is cleared, a new turn must not open: two open turns for one conversation
    /// would let the older one's tools keep writing.
    unconfirmed_turn: Option<String>,
    /// Set once a guardian refused this host. Kept so later turns do not keep
    /// asking, and so the refusal is reported instead of silently degrading.
    refused: Option<String>,
    /// The foreground authority for this conversation, asked once.
    ///
    /// `None` means "not asked yet". Asking once matches the guardian's contract
    /// (a bind, not a turn) and keeps a transient refusal from being retried into
    /// a different answer.
    authority: Option<ForegroundAuthority>,
    /// The run id a managed authorization was minted for.
    ///
    /// `authorization.client_run_id` has to equal `PrepareSession.host_run_id`, so a
    /// managed host must be built with *this* id, not a random one.
    managed_run_id: Option<String>,
}

impl ThreadDelegation {
    /// Builds the driver for one conversation.
    ///
    /// The identity directory is supplied by the host rather than derived here:
    /// the same conversation must find the same secret after a restart, and only
    /// the thread store knows where this workspace keeps it.
    pub(crate) fn new(root: Arc<ThreadRoot>, identities: SessionIdentityStore) -> Self {
        Self {
            root,
            identities,
            host: None,
            secret: None,
            unconfirmed_turn: None,
            refused: None,
            authority: None,
            managed_run_id: None,
        }
    }

    /// Builds the driver from the thread id and the store's identity directory.
    ///
    /// Failing to load a *damaged* credential is reported as `Unavailable`: it is
    /// not a decision about this conversation, and silently minting a replacement
    /// would let a corrupted file claim a container it does not own.
    pub(crate) fn for_thread(root: Arc<ThreadRoot>, identity_directory: &Path) -> Self {
        let identities = SessionIdentityStore::new(identity_directory.to_path_buf());
        Self::new(root, identities)
    }

    /// Asks the runtime whether this host may act as a managed session, once.
    ///
    /// The answer is cached because it is a property of the host (a lease), not of
    /// the turn; re-asking per turn would let a transient refusal be retried until
    /// it changed, which is exactly how a policy "no" turns into a silent "yes".
    pub(crate) fn resolve_authority(
        &mut self,
        manager: &crate::server::foreground::ForegroundDispatch,
    ) -> Result<(), DelegationFailure> {
        if self.authority.is_some() {
            return Ok(());
        }
        let operation_id = format!("app-server-delegation:{}", std::process::id());
        let authority = manager
            .authority(&self.root, &operation_id)
            .map_err(DelegationFailure::Unavailable)?;
        match &authority {
            ForegroundAuthority::Managed(authorization) => {
                self.managed_run_id = Some(authorization.client_run_id.clone());
            }
            ForegroundAuthority::Refused(reason) => self.refused = Some(reason.clone()),
            ForegroundAuthority::Embedded => {}
        }
        self.authority = Some(authority);
        Ok(())
    }

    /// Prepares this turn and returns the client its tools must hold.
    ///
    /// Order matters and is the same on every path: prepare the binding, open the
    /// turn, and only then hand back a client. A caller that gets `Ready` can spawn;
    /// a caller that gets `NotReady` runs the conversation without delegation and
    /// reports why.
    /// Prepares this turn and returns the client its tools must hold.
    ///
    /// `manager` is the process's [`ForegroundDispatch`]; without it this is an
    /// embedded deployment (the TUI's fixed binding, a daemon worker, a test) and
    /// the host's own resume secret is the identity.
    pub(crate) fn prepare_turn_with(
        &mut self,
        turn_id: &str,
        manager: Option<&crate::server::foreground::ForegroundDispatch>,
    ) -> PreparedTurn {
        if let Some(unclosed) = self.unconfirmed_turn.as_deref() {
            // Never open a second turn while the previous one is unaccounted for:
            // its tools may still be mid-call, and the daemon would replace it
            // underneath them.
            return PreparedTurn::NotReady(DelegationFailure::Unavailable(format!(
                "turn {unclosed} was never confirmed closed"
            )));
        }
        // A guardian that refused is a decision about this host, not a missing
        // feature: delegation is refused and the conversation keeps running. Falling
        // back to the resume secret here would answer "no" with "yes".
        if let (Some(manager), Some(reason)) = (manager, self.refused.as_deref()) {
            let _ = manager;
            return PreparedTurn::NotReady(DelegationFailure::Refused(SessionFailure::new(
                yi_agent_lifecycle::session_delegation::SessionFailureCode::AuthorityDenied,
                format!("the runtime refused this host's delegation authority: {reason}"),
            )));
        }
        let secret = match self.identity() {
            Ok(secret) => secret,
            Err(failure) => return PreparedTurn::NotReady(failure),
        };
        // The digest is sent on every attach, not only the first: the daemon records
        // it **create-only** (`WHERE resume_digest IS NULL`), so re-sending is a
        // no-op on an existing container and is what lets a container created by an
        // earlier build -- or by a run that lost the reply -- still learn the
        // credential this conversation now holds. Sending `None` instead would leave
        // a brand-new container with no digest, and the very next `prepare` would be
        // refused with `SessionIdentityRequired`.
        // The attach names the identity the daemon will record. A managed host
        // presents the guardian's authorization; an embedded one presents its own
        // secret. The *digest* is only ever the embedded resume secret: a managed
        // container is authorized by the ledger, not by a stored hash.
        let identity = match (manager, self.authority.as_ref()) {
            (Some(_), Some(ForegroundAuthority::Managed(authorization))) => {
                SessionIdentity::Managed(authorization.clone())
            }
            _ => SessionIdentity::Embedded(secret.clone()),
        };
        let creation_digest = match &identity {
            SessionIdentity::Managed(_) => None,
            _ => Some(session_resume_digest(&secret)),
        };
        let attached = match self
            .root
            .attach_session_container(&identity, turn_id, creation_digest)
        {
            Ok(attached) => attached,
            Err(error) => {
                return PreparedTurn::NotReady(DelegationFailure::from_client(error));
            }
        };
        // A host is reused across turns: a new `SessionHost` per turn would look like
        // a driver replacement every time and demand a stop proof that has nothing to
        // prove. It is rebuilt when the daemon generation moved -- a restarted daemon
        // has no binding for the old host, and the stale binding would otherwise be
        // re-sent forever as "the current one".
        let host_needs_rebuild = match self.host.as_ref() {
            None => true,
            Some(host) => host.attached().generation != attached.generation,
        };
        if host_needs_rebuild {
            // A managed binding is verified against the authorization's own
            // `client_run_id`, so the host must carry exactly that id. Reusing a
            // random one would be refused as "another root or host".
            let (identity, run_id) = match (&identity, self.managed_run_id.clone()) {
                (SessionIdentity::Managed(authorization), Some(run_id)) => {
                    (SessionIdentity::Managed(authorization.clone()), run_id)
                }
                _ => (
                    SessionIdentity::Embedded(secret),
                    format!("host-{}", uuid::Uuid::new_v4()),
                ),
            };
            self.host = Some(SessionHost::new_for_run(
                Arc::clone(&self.root),
                identity,
                None,
                attached,
                run_id,
            ));
        }
        let host = self.host.as_mut().expect("a host was just built");
        if let Err(error) = host.prepare(turn_id) {
            // A stale binding after a restart is exactly what `prepare` fixes, so a
            // failure here is reported but the host is kept: the next turn retries
            // against the same root rather than rebuilding from scratch.
            return PreparedTurn::NotReady(DelegationFailure::from_client(error));
        }
        let turn = match host.begin_turn(turn_id) {
            Ok(turn) => turn,
            Err(error) => return PreparedTurn::NotReady(DelegationFailure::from_client(error)),
        };
        // Register *before* the tools exist. The durable `starting` row is what
        // keeps the next driver honest if this process dies mid-turn: without it a
        // successor would see "nothing was running" and take over while this turn's
        // tools might still be writing. Failing here therefore aborts the turn's
        // delegation rather than proceeding unregistered.
        match host.register_execution(turn_id) {
            Ok(execution) => PreparedTurn::Ready {
                turn: Arc::new(turn),
                execution,
            },
            Err(error) => {
                // The turn opened but cannot be accounted for: close it, so a
                // registered-nothing turn does not linger as an open write window.
                turn.close_locally();
                let _ = host.finish_turn(&turn);
                PreparedTurn::NotReady(DelegationFailure::from_client(error))
            }
        }
    }

    /// Reports what this driver actually observed about its execution's stop.
    ///
    /// Both facts are observations and neither is assumed: `joined` says the run's
    /// real root task was awaited to completion, and `tools_stopped` (from the
    /// caller's process manager) says every process group the turn left behind is
    /// gone. This module cannot see tool processes, so it must not guess them.
    ///
    /// Passing a `false` is legitimate and useful: the daemon refuses the record,
    /// the execution stays unsettled, and the next driver is told "unproven"
    /// instead of being handed a session whose old turn may still be writing.
    pub(crate) fn record_stopped(
        &mut self,
        execution: &ExecutionIdentity,
        joined: bool,
        tools_stopped: bool,
    ) -> Result<(), SessionClientError> {
        let Some(host) = self.host.as_ref() else {
            return Err(SessionClientError::Transport(
                "no session host is prepared, so no execution can be retired".to_string(),
            ));
        };
        let Some(fence) = host.binding_fence() else {
            return Err(SessionClientError::Protocol(
                "the session host holds no binding, so no execution can be retired".to_string(),
            ));
        };
        host.record_stopped(StopEvidence {
            evidence_id: format!("host:{}:{}", execution.execution_id, fence.driver_epoch),
            fence: ForegroundFence {
                root: RootRef {
                    session_id: fence.root.session_id.clone(),
                    root_id: fence.root.root_id.clone(),
                    attempt_id: execution.attempt_id.clone(),
                },
                generation: fence.generation.clone(),
                binding_epoch: fence.binding_epoch,
                driver_epoch: fence.driver_epoch,
            },
            execution: execution.clone(),
            attempt_id: execution.attempt_id.clone(),
            source: EvidenceSource::Host,
            joined,
            tools_stopped,
            process_identity: None,
        })
    }

    /// Closes the turn, recording an unconfirmed close so the next turn refuses.
    pub(crate) fn finish_turn(&mut self, turn: &SessionTurnClient) {
        let Some(host) = self.host.as_mut() else {
            return;
        };
        // Local close first, always, so this turn's tools stop even if the daemon
        // never answers.
        turn.close_locally();
        match host.finish_turn(turn) {
            Ok(()) => self.unconfirmed_turn = None,
            Err(error) => {
                self.unconfirmed_turn = Some(turn.context().turn_id.clone());
                tracing::warn!(
                    thread_id = %self.root.thread_id(),
                    turn_id = %turn.context().turn_id,
                    %error,
                    "the daemon did not confirm this turn's close"
                );
            }
        }
    }

    /// Loads (or mints) this conversation's resume secret.
    fn identity(&mut self) -> Result<SecretCapability, DelegationFailure> {
        if let Some(secret) = self.secret.as_ref() {
            return Ok(secret.clone());
        }
        let secret = self
            .identities
            .load_or_create(self.root.thread_id())
            .map_err(|error| {
                DelegationFailure::Unavailable(format!(
                    "the conversation's resume credential is unusable: {error}"
                ))
            })?;
        self.secret = Some(secret.clone());
        Ok(secret)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A refused turn is never reported as ready, and keeps the daemon's reason.
    #[test]
    fn a_failure_is_reported_rather_than_degraded_silently() {
        let refused =
            DelegationFailure::from_client(SessionClientError::Rejected(SessionFailure::new(
                yi_agent_lifecycle::session_delegation::SessionFailureCode::SessionClosed,
                "closed by the user",
            )));
        let prepared = PreparedTurn::NotReady(refused);
        assert!(prepared.failure().is_some());
        assert!(matches!(
            prepared.failure(),
            Some(DelegationFailure::Refused(failure)) if failure.reason == "closed by the user"
        ));
    }

    /// A guardian's refusal is a decision, not a missing feature.
    ///
    /// The bug this pins: `authorize_foreground_root` used to answer `None` for
    /// both "no guardian" and "the guardian refused", so a refused host fell back
    /// to its embedded resume secret. A policy "no" then became permission. The
    /// three states are now distinct and a refusal keeps delegation unavailable.
    #[test]
    fn a_refused_authority_is_not_downgraded_to_embedded() {
        assert_eq!(
            ForegroundAuthority::Refused("policy said no".into()).into_authorization(),
            None,
            "a refusal must not yield an authorization"
        );
        assert_eq!(
            ForegroundAuthority::Embedded.into_authorization(),
            None,
            "embedded hosts authorize nothing at the guardian"
        );
        // The two are still distinguishable, which is the point: only the refusal
        // has a reason, and only the managed case can spawn as managed.
        assert_ne!(
            ForegroundAuthority::Embedded,
            ForegroundAuthority::Refused("policy said no".into())
        );
    }

    /// The identity directory is a sibling of `threads/`, not inside it, so a
    /// forked thread cannot inherit a credential by copying a directory tree.
    ///
    /// Asserted on the store, which is the one place production reads it: a second
    /// implementation here could drift and quietly put credentials inside a
    /// directory `fork_thread` copies.
    #[test]
    fn the_identity_directory_sits_beside_threads() {
        let store = crate::thread_store::ThreadStore::new(Path::new("/project"));
        let directory = store.session_identity_directory();
        assert_eq!(directory, Path::new("/project/.session-identities"));
        assert!(
            !directory.starts_with(store.root()),
            "a credential inside the threads root would be copied by a fork"
        );
    }
}
