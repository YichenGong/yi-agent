//! Per-turn session delegation for every host that drives a conversation.
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
//! The app-server was one such host; the TUI and `run` are the others, and they
//! need exactly the same ordering. So the machinery lives here, in the subagent
//! crate both of them already depend on, and each host keeps only its own wire.
//!
//! This module makes delegation a **per-turn** fact. Each turn prepares the
//! container binding, opens a turn, installs the turn's own tool set through
//! [`yi_agent_core::Agent::set_tools_for_next_run`], and closes the turn on every
//! exit path. The binding is reused across turns; only the turn and the secrets
//! change.
//!
//! Failure is reported, never silently swallowed: a turn whose delegation could
//! not be prepared still runs (the conversation must stay usable), but the caller
//! is handed the failure so "I could not spawn" is a visible state rather than a
//! mystery.

use std::path::Path;
use std::sync::Arc;

use crate::session_delegation::{SessionClientError, SessionHost, SessionTurnClient};
use crate::session_identity::SessionIdentityStore;
use crate::thread_root::ForegroundAuthority;
use crate::thread_root::ThreadRoot;
use yi_agent_lifecycle::foreground::{
    EvidenceSource, ExecutionIdentity, ForegroundFence, RootRef, SecretCapability, StopEvidence,
};
use yi_agent_lifecycle::session_delegation::{
    SessionFailure, SessionIdentity, session_resume_digest,
};

/// Why a turn could not be prepared for delegation.
///
/// Only the first variant says something about the conversation; the second says
/// this build/daemon pair cannot offer the contract at all. Keeping them apart
/// matters because the first is worth retrying next turn and the second is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DelegationFailure {
    /// The daemon refused, or the identity did not check out.
    Refused(SessionFailure),
    /// No usable runtime, or the two sides disagree about the protocol.
    Unavailable(String),
}

impl DelegationFailure {
    /// The failure's stable code, for a host that reports it on its own wire.
    ///
    /// Serialized rather than formatted: the daemon already names codes in
    /// `snake_case`, so a client can branch on one without a second mapping.
    pub fn code(&self) -> String {
        match self {
            Self::Refused(failure) => serde_json::to_value(failure.code)
                .ok()
                .and_then(|value| value.as_str().map(str::to_string))
                .unwrap_or_else(|| "protocol_mismatch".to_string()),
            Self::Unavailable(_) => "unavailable".to_string(),
        }
    }

    /// The human half of the failure.
    pub fn reason(&self) -> &str {
        match self {
            Self::Refused(failure) => &failure.reason,
            Self::Unavailable(reason) => reason,
        }
    }

    /// Wraps a daemon decision, for a host that never saw the raw client error.
    pub fn from_rejected(failure: SessionFailure) -> Self {
        Self::Refused(failure)
    }

    /// Converts a client error: a rejection is a decision, anything else is a gap
    /// in this build or the connection.
    pub fn from_client(error: SessionClientError) -> Self {
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
pub enum PreparedTurn {
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
    pub fn failure(&self) -> Option<&DelegationFailure> {
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

/// The five tool names a turn's delegation replaces.
///
/// Named once, because every host has to strip them: a registry assembled before
/// the first turn (or before the runtime attached) still holds the legacy
/// instances, and leaving them in place is exactly how a settled history kept
/// answering `spawn_agent` after delegation should have stopped.
pub const DELEGATION_TOOL_NAMES: [&str; 5] = [
    "spawn_agent",
    "send_message",
    "wait_agent",
    "inspect_agent",
    "cancel_agent",
];

/// Installs a turn's capability into `registry`, replacing whatever was there.
///
/// Takes the prepared turn by value so a `NotReady` cannot be mistaken for one
/// that is merely unused: on failure nothing is registered and the conversation
/// runs with no delegation tools at all.
pub fn install_turn_tools(
    registry: &mut yi_agent_core::ToolRegistry,
    prepared: PreparedTurn,
    controller: yi_agent_tools::SandboxController,
    thread_id: Option<String>,
    caller: crate::CallerContext,
) -> Result<(Arc<SessionTurnClient>, ExecutionIdentity), DelegationFailure> {
    for name in DELEGATION_TOOL_NAMES {
        registry.remove(name);
    }
    match prepared {
        PreparedTurn::Ready { turn, execution } => {
            crate::register_session_turn_tools(
                registry,
                Arc::clone(&turn),
                controller,
                thread_id,
                caller,
            );
            Ok((turn, execution))
        }
        PreparedTurn::NotReady(failure) => Err(failure),
    }
}

/// The delegation driver for one conversation.
///
/// Holds the host (which owns the binding secret) for as long as the thread lives.
/// The host is what makes a restart recoverable: the resume secret is on disk, so
/// a new process can prove it is the same conversation instead of creating a
/// second container.
pub struct ThreadDelegation {
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
    pub fn new(root: Arc<ThreadRoot>, identities: SessionIdentityStore) -> Self {
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
    pub fn for_thread(root: Arc<ThreadRoot>, identity_directory: &Path) -> Self {
        let identities = SessionIdentityStore::new(identity_directory.to_path_buf());
        Self::new(root, identities)
    }

    /// Records the authority this host resolved, asking nothing itself.
    ///
    /// Asking is the caller's job because only the caller holds the guardian
    /// connection. Recording is idempotent: the first answer stands, so a
    /// transient refusal cannot be retried until it changes into permission.
    pub fn set_authority(&mut self, authority: ForegroundAuthority) {
        if self.authority.is_some() {
            return;
        }
        match &authority {
            ForegroundAuthority::Managed(authorization) => {
                self.managed_run_id = Some(authorization.client_run_id.clone());
            }
            ForegroundAuthority::Refused(reason) => self.refused = Some(reason.clone()),
            ForegroundAuthority::Embedded => {}
        }
        self.authority = Some(authority);
    }

    /// The conversation this driver serves.
    ///
    /// Exposed so a host can ask its guardian about *this* root without keeping a
    /// second copy of the handle -- two copies could drift into asking about the
    /// wrong conversation, which is the one thing the identity check forbids.
    pub fn root(&self) -> Arc<ThreadRoot> {
        Arc::clone(&self.root)
    }

    /// The authority recorded so far, if the caller has asked.
    pub fn authority(&self) -> Option<&ForegroundAuthority> {
        self.authority.as_ref()
    }

    /// Prepares this turn and returns the client its tools must hold.
    ///
    /// Order matters and is the same on every path: prepare the binding, open the
    /// turn, and only then hand back a client. A caller that gets `Ready` can spawn;
    /// a caller that gets `NotReady` runs the conversation without delegation and
    /// reports why.
    ///
    /// No recorded authority means an embedded deployment: the host's own resume
    /// secret is the identity, which is what a TUI or a daemon worker has.
    pub fn prepare_turn(&mut self, turn_id: &str) -> PreparedTurn {
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
        if let Some(reason) = self.refused.as_deref() {
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
        let identity = match self.authority.as_ref() {
            Some(ForegroundAuthority::Managed(authorization)) => {
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
    pub fn record_stopped(
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
    pub fn finish_turn(&mut self, turn: &SessionTurnClient) {
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

    /// A credential must never live inside the directory a fork copies.
    ///
    /// Asserted on the subagent store itself, which is what every host uses to
    /// find its secret: a host-side path helper could drift and quietly put
    /// credentials where `fork_thread` would copy them to the child.
    #[test]
    fn the_identity_store_never_writes_inside_a_transcript_directory() {
        let directory = tempfile::tempdir().expect("a temp dir");
        let store = SessionIdentityStore::new(directory.path().join(".session-identities"));
        let secret = store
            .load_or_create("thread-1")
            .expect("a minted credential");
        let path = store.directory().join("thread-1.json");
        assert!(
            path.exists(),
            "the secret was written where the store says it is"
        );
        assert!(
            !store
                .directory()
                .starts_with(directory.path().join("threads")),
            "never inside the transcripts a fork copies"
        );
        assert_eq!(store.load_or_create("thread-1").expect("stable"), secret);
    }
}
