//! The guardian's half of the private authority bridge.
//!
//! Two responsibilities, and they are deliberately in one file so the invariant they
//! share is visible:
//!
//! 1. **Root authority that the guardian is willing to vouch for.** A root may only be
//!    authorized by a client that holds a live `Business` lease; a `LeaseOnly`
//!    observer, a released client, and a tool process all fail the same check.
//! 2. **Answering the daemon's challenges.** The daemon owns the clock (see
//!    `BridgeFrame::Challenge`), so this side only answers questions it was asked, and
//!    only ever with the sequence number the question named or a higher one.
//!
//! The capabilities here are per root and generated at authorization time. Nothing is
//! derived from a thread key: the key is a lookup, and possession of it buys nothing.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use yi_agent_lifecycle::deadline::Deadline;
use yi_agent_lifecycle::foreground::{
    AuthorityMessage, AuthorityRefusal, AuthorityStamp, BridgeFrame, RootAuthorityScope,
    RootBindIntent,
};
use yi_agent_lifecycle::types::RootAuthorization;

use super::guardian::Guardian;

/// Everything needed to authorize one root.
///
/// A struct rather than seven positional arguments: the fields are all strings that
/// look alike (scope key, root id, generation, owner), and a caller that swapped two of
/// them would compile and then authorize the wrong root.
pub struct AuthorizeRequest<'a> {
    pub intent: &'a RootBindIntent,
    pub root_id: &'a str,
    pub client_run_id: &'a str,
    pub scope_key: &'a str,
    pub generation: &'a str,
    /// The owner an already-bound root reports, when there is one.
    pub existing_owner: Option<&'a str>,
    pub scope: RootAuthorityScope,
}

/// One root the guardian has vouched for.
#[derive(Clone, Debug)]
pub struct GrantedRoot {
    pub root_id: String,
    pub scope_key: String,
    pub client_run_id: String,
    pub binding_epoch: u64,
    pub scope: RootAuthorityScope,
    /// The `<redacted>`-printing capability handed to the host.
    pub capability: yi_agent_lifecycle::foreground::SecretCapability,
    /// The last sequence this guardian issued, so a re-answer never goes backwards.
    pub last_authority_seq: u64,
}

impl GrantedRoot {
    /// Whether this grant still speaks for `client_run_id`.
    pub fn belongs_to(&self, client_run_id: &str) -> bool {
        self.client_run_id == client_run_id
    }
}

/// The guardian's authorization table plus the answers it owes the daemon.
#[derive(Debug, Default)]
pub struct RootAuthorityTable {
    granted: HashMap<String, GrantedRoot>,
    /// Roots whose authorization was withdrawn, with the sequence that withdrew it.
    ///
    /// Kept after the grant is gone because the *daemon* still has to be told, and a
    /// revocation that could not be delivered must be retried rather than forgotten.
    revoked: HashMap<String, u64>,
    /// Roots reserved by an in-flight prepare, keyed by their operation id.
    reserved: HashMap<String, ReservedRoot>,
}

#[derive(Clone, Debug)]
struct ReservedRoot {
    root_id: String,
    workspace: std::path::PathBuf,
}

impl RootAuthorityTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Roots currently authorized for a client.
    pub fn roots_of(&self, client_run_id: &str) -> Vec<String> {
        self.granted
            .values()
            .filter(|grant| grant.belongs_to(client_run_id))
            .map(|grant| grant.root_id.clone())
            .collect()
    }

    pub fn grant(&self, root_id: &str) -> Option<&GrantedRoot> {
        self.granted.get(root_id)
    }

    /// Withdraws every root a client owned, returning them with the sequence that
    /// withdrew them.
    ///
    /// The sequence is one past the last one issued, so any message generated before
    /// this decision is refused by the daemon even if it arrives afterwards.
    pub fn revoke_owner(&mut self, client_run_id: &str) -> Vec<(String, u64)> {
        let roots: Vec<String> = self.roots_of(client_run_id);
        let mut revoked = Vec::new();
        for root in roots {
            if let Some(grant) = self.granted.remove(&root) {
                let seq = grant.last_authority_seq.saturating_add(1);
                self.revoked.insert(root.clone(), seq);
                revoked.push((root, seq));
            }
        }
        revoked
    }

    /// Grants authority for `intent`, or refuses.
    ///
    /// `existing_owner` is what an already-bound root reports: a root bound to a live
    /// client may not be taken over by another one, even with a valid lease. That is
    /// the check that stops "guess the thread key" from becoming "steal the session".
    pub fn authorize(&mut self, request: &AuthorizeRequest<'_>) -> Result<GrantedRoot, String> {
        let AuthorizeRequest {
            intent,
            root_id,
            client_run_id,
            scope_key,
            generation,
            existing_owner,
            scope,
        } = *request;
        if existing_owner.is_some_and(|owner| owner != client_run_id) {
            return Err(format!(
                "root {root_id} is already owned by another live client; \
                 a different client cannot take it over"
            ));
        }
        // A revoked root may come back, but only with a sequence above the revocation.
        // Starting at `revoked + 1` is what makes a queued frame for the old authority
        // lose against the new one instead of racing it.
        let binding_epoch = self
            .granted
            .get(root_id)
            .map(|grant| grant.binding_epoch.saturating_add(1))
            .unwrap_or(1);
        let capability = yi_agent_lifecycle::foreground::SecretCapability::new(format!(
            "root-{}-{}-{}",
            root_id,
            generation,
            new_capability_nonce()
        ));
        let grant = GrantedRoot {
            root_id: root_id.to_string(),
            scope_key: scope_key.to_string(),
            client_run_id: client_run_id.to_string(),
            binding_epoch,
            scope,
            capability,
            last_authority_seq: self
                .revoked
                .get(root_id)
                .map_or(1, |seq| seq.saturating_add(1)),
        };
        self.granted.insert(root_id.to_string(), grant.clone());
        self.reserved.insert(
            intent.operation_id.clone(),
            ReservedRoot {
                root_id: root_id.to_string(),
                workspace: intent.workspace.clone(),
            },
        );
        Ok(grant)
    }

    /// The reservation an operation id already holds, if any.
    ///
    /// Used to make a retried prepare idempotent: the same operation must resolve to the
    /// same root rather than creating a second one.
    pub fn reservation(&self, operation_id: &str) -> Option<String> {
        self.reserved
            .get(operation_id)
            .map(|reserved| reserved.root_id.clone())
    }

    /// The workspace a reserved operation was created for.
    pub fn reservation_workspace(&self, operation_id: &str) -> Option<&std::path::Path> {
        self.reserved
            .get(operation_id)
            .map(|reserved| reserved.workspace.as_path())
    }

    /// The authorization the host was given for a root.
    pub fn authorization_for(
        &self,
        root_id: &str,
        daemon_instance_id: &str,
    ) -> Option<RootAuthorization> {
        let grant = self.granted.get(root_id)?;
        Some(RootAuthorization {
            scope_key: grant.scope_key.clone(),
            daemon_instance_id: daemon_instance_id.to_string(),
            client_run_id: grant.client_run_id.clone(),
            root_id: grant.root_id.clone(),
            capability: grant.capability.expose().to_string(),
        })
    }

    /// Answers one daemon challenge with the authority this guardian holds.
    ///
    /// The answer's sequence is the asked-for one or higher, never lower: a reply below
    /// the question would be refused by the daemon anyway, and replying high is what
    /// makes the withdrawal sequence authoritative.
    pub fn answer_challenge(
        &mut self,
        nonce: &str,
        root_id: &str,
        authority_seq: u64,
        generation: &str,
        remaining_lease: Duration,
        daemon_instance_id: &str,
    ) -> BridgeFrame {
        let Some(grant) = self.granted.get_mut(root_id) else {
            return BridgeFrame::Refused {
                nonce: nonce.to_string(),
                reason: AuthorityRefusal::NoOwner,
            };
        };
        let seq = authority_seq.max(grant.last_authority_seq);
        grant.last_authority_seq = seq;
        let stamp = AuthorityStamp {
            scope_key: grant.scope_key.clone(),
            generation: generation.to_string(),
            owner: grant.client_run_id.clone(),
            root_id: grant.root_id.clone(),
            binding_epoch: grant.binding_epoch,
            authority_seq: seq,
            challenge_nonce: nonce.to_string(),
            remaining_lease_ms: u64::try_from(remaining_lease.as_millis()).unwrap_or(u64::MAX),
            authority_scope: grant.scope,
        };
        let _ = daemon_instance_id;
        BridgeFrame::Authority(AuthorityMessage::Renew { stamp })
    }
}

fn new_capability_nonce() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    format!("{nanos:x}{:x}", std::process::id())
}

impl Guardian {
    /// The guardian's root authority table.
    ///
    /// Named `roots` rather than `root_authority` because the field has that name and a
    /// method that shadows it would turn `self.root_authority` into a call in every
    /// future edit — a trap nobody should have to remember.
    pub fn roots(&self) -> &RootAuthorityTable {
        &self.root_authority
    }

    pub fn roots_mut(&mut self) -> &mut RootAuthorityTable {
        &mut self.root_authority
    }

    /// Installs the guardian's end of the private authority bridge.
    ///
    /// Takes the stream rather than a borrowed one because the bridge has to outlive
    /// the call that set it up: a local would close the descriptor and tell the daemon
    /// its guardian is gone.
    pub fn attach_bridge(&mut self, bridge: std::os::unix::net::UnixStream) {
        self.bridge = Some(bridge);
    }

    pub fn bridge_mut(&mut self) -> Option<&mut std::os::unix::net::UnixStream> {
        self.bridge.as_mut()
    }

    /// Whether a bridge is currently attached.
    pub fn has_bridge(&self) -> bool {
        self.bridge.is_some()
    }

    /// Authorizes one root for a client that has already been authenticated.
    ///
    /// The lease check happens here, not in the caller: every path that can mint root
    /// authority must go through the same gate, or the gate is decorative.
    ///
    /// Returns the wire authorization because that is what every caller answers with.
    /// The internal grant (which owns the capability) never leaves this module.
    pub fn authorize_root(
        &mut self,
        grant: &yi_agent_lifecycle::types::ClientGrant,
        request: &AuthorizeRequest<'_>,
        now: std::time::Duration,
    ) -> std::io::Result<yi_agent_lifecycle::types::RootAuthorization> {
        self.validate_business_lease(grant, now)?;
        let request = AuthorizeRequest {
            client_run_id: &grant.client_run_id,
            ..*request
        };
        let granted = self
            .roots_mut()
            .authorize(&request)
            .map_err(|reason| std::io::Error::new(std::io::ErrorKind::PermissionDenied, reason))?;
        Ok(yi_agent_lifecycle::types::RootAuthorization {
            scope_key: granted.scope_key,
            daemon_instance_id: request.generation.to_string(),
            client_run_id: granted.client_run_id,
            root_id: granted.root_id,
            capability: granted.capability.expose().to_string(),
        })
    }

    /// The one place a client proves it may hold root authority.
    ///
    /// `LeaseOnly` is refused with the same shape as "no lease": the mode never
    /// authorizes work, and telling the two apart in the error would help nobody but a
    /// caller probing which refusals are which.
    fn validate_business_lease(
        &self,
        grant: &yi_agent_lifecycle::types::ClientGrant,
        now: std::time::Duration,
    ) -> std::io::Result<()> {
        self.reclaimer()
            .leases()
            .admits(grant, now)
            .map_err(|reason| std::io::Error::new(std::io::ErrorKind::PermissionDenied, reason))
    }

    /// Withdraws every root a client owned and tells the daemon, in that order.
    ///
    /// Revoking locally first is what makes the withdrawal authoritative: a message
    /// that later arrives at the daemon is refused by its tombstone, not by a race with
    /// this table. The daemon write can then fail without the local state disagreeing
    /// about who may admit work.
    ///
    /// Returns an error when the daemon could not be told and there *was* authority to
    /// withdraw. A caller that reported success anyway would leave the daemon admitting
    /// a client the guardian has already forgotten.
    pub fn withdraw_root_authority(
        &mut self,
        client_run_id: &str,
    ) -> std::io::Result<Option<BridgeFrame>> {
        let revoked = self.roots_mut().revoke_owner(client_run_id);
        if revoked.is_empty() {
            return Ok(None);
        }
        let frame = revocation_frame(client_run_id, &revoked, &self.generation.to_string());
        if let Some(frame) = frame.clone() {
            self.send_bridge_frame(&frame)?;
        }
        Ok(frame)
    }

    /// Sends one frame over the private bridge, when this guardian has one.
    ///
    /// A guardian with no bridge (tests, and a runtime whose daemon was never started)
    /// has nothing to tell, so the send is a no-op rather than an error: the frames
    /// exist to describe authority *to* the daemon, and with no daemon there is nobody
    /// who could be admitting work.
    pub fn send_bridge_frame(&mut self, frame: &BridgeFrame) -> std::io::Result<()> {
        let Some(stream) = self.bridge.as_mut() else {
            return Ok(());
        };
        yi_agent_lifecycle::codec::write_frame(stream, frame)
    }
}

/// The deadline a bridge answer must arrive within.
///
/// The daemon uses [`yi_agent_store::ipc::authority::CHALLENGE_TTL`]; the guardian must
/// answer well inside it or the daemon will (correctly) refuse a late answer.
pub const BRIDGE_ANSWER_BUDGET: Duration = Duration::from_secs(2);

/// A challenge the guardian has seen, for logging and for the ordering assertion.
#[derive(Clone, Debug)]
pub struct SeenChallenge {
    pub nonce: String,
    pub root_id: String,
    pub authority_seq: u64,
    pub seen_at: Instant,
}

impl SeenChallenge {
    pub fn new(nonce: &str, root_id: &str, authority_seq: u64) -> Self {
        Self {
            nonce: nonce.to_string(),
            root_id: root_id.to_string(),
            authority_seq,
            seen_at: Instant::now(),
        }
    }
}

/// Answers a frame, or explains why it cannot.
///
/// Kept as a free function so the socket loop stays a loop: the parsing and the policy
/// are testable without a socket, and the loop's only job is to move bytes.
pub fn answer(
    table: &mut RootAuthorityTable,
    frame: &BridgeFrame,
    generation: &str,
    daemon_instance_id: &str,
    remaining_lease: Duration,
) -> Option<BridgeFrame> {
    match frame {
        BridgeFrame::Challenge {
            nonce,
            root_id,
            authority_seq,
        } => Some(table.answer_challenge(
            nonce,
            root_id,
            *authority_seq,
            generation,
            remaining_lease,
            daemon_instance_id,
        )),
        // The daemon never asks the guardian to answer an authority frame; if it does,
        // the peer is confused and silence is the honest reply. The same goes for the
        // frames only the daemon may originate: answering one would be a guardian
        // inventing a commit.
        BridgeFrame::Authority(_)
        | BridgeFrame::Refused { .. }
        | BridgeFrame::Prepare { .. }
        | BridgeFrame::Committed { .. }
        | BridgeFrame::Denied { .. } => None,
    }
}

/// Builds the revocation frame a release owes the daemon.
///
/// One `RevokeOwner` per client rather than one frame per root: the daemon withdraws
/// by owner, and a per-root message stream would leave a window where some roots of a
/// gone client were still admitted. The sequence is the highest of the per-root
/// withdrawals, so the frame also outranks anything the client had already queued.
///
/// `None` when there is nothing to revoke, which is not a failure: a client that never
/// held a root owes the daemon no message.
pub fn revocation_frame(
    client_run_id: &str,
    revoked: &[(String, u64)],
    generation: &str,
) -> Option<BridgeFrame> {
    let seq = revoked.iter().map(|(_, seq)| *seq).max()?;
    Some(BridgeFrame::Authority(AuthorityMessage::RevokeOwner {
        owner: client_run_id.to_string(),
        generation: generation.to_string(),
        authority_seq: seq,
    }))
}

/// Answers one challenge with a refusal rather than an installation.
pub fn refusal(nonce: &str, reason: AuthorityRefusal) -> BridgeFrame {
    BridgeFrame::Refused {
        nonce: nonce.to_string(),
        reason,
    }
}

/// A deadline for reading one bridge answer, derived from the caller's budget.
pub fn answer_deadline(budget: Duration) -> Deadline {
    Deadline::after(budget.min(BRIDGE_ANSWER_BUDGET))
}

#[cfg(test)]
mod tests {
    use super::*;
    use yi_agent_lifecycle::foreground::AuthorityMessage;

    fn intent(operation: &str, thread: &str, workspace: &str) -> RootBindIntent {
        RootBindIntent {
            operation_id: operation.into(),
            thread_key: thread.into(),
            workspace: workspace.into(),
            extra_roots: vec![],
            previous_root_capability: None,
        }
    }

    #[test]
    fn foreground_root_lifecycle_authorize_mints_a_capability_not_derivable_from_the_key() {
        let mut table = RootAuthorityTable::new();
        let granted = table
            .authorize(&AuthorizeRequest {
                intent: &intent("op-1", "thread:a", "/tmp/a"),

                root_id: "root-1",

                client_run_id: "run-1",

                scope_key: "scope",

                generation: "gen-1",

                existing_owner: None,

                scope: RootAuthorityScope::Business,
            })
            .unwrap();
        let exposed = granted.capability.expose();
        assert_ne!(exposed, "thread:a");
        assert!(!exposed.contains("thread:a"));
        // The debug rendering never contains it either: this value is logged.
        assert!(!format!("{granted:?}").contains(exposed));
    }

    #[test]
    fn foreground_root_lifecycle_another_client_cannot_take_over_a_live_root() {
        let mut table = RootAuthorityTable::new();
        let error = table
            .authorize(&AuthorizeRequest {
                intent: &intent("op-2", "thread:a", "/tmp/a"),

                root_id: "root-1",

                client_run_id: "run-2",

                scope_key: "scope",

                generation: "gen-1",

                existing_owner: Some("run-1"),

                scope: RootAuthorityScope::Business,
            })
            .unwrap_err();
        assert!(error.contains("cannot take it over"), "{error}");
        assert!(table.grant("root-1").is_none());
    }

    #[test]
    fn foreground_root_lifecycle_the_same_owner_may_renew_its_own_root() {
        let mut table = RootAuthorityTable::new();
        table
            .authorize(&AuthorizeRequest {
                intent: &intent("op-1", "thread:a", "/tmp/a"),

                root_id: "root-1",

                client_run_id: "run-1",

                scope_key: "scope",

                generation: "gen-1",

                existing_owner: None,

                scope: RootAuthorityScope::Business,
            })
            .unwrap();
        let regranted = table
            .authorize(&AuthorizeRequest {
                intent: &intent("op-1", "thread:a", "/tmp/a"),

                root_id: "root-1",

                client_run_id: "run-1",

                scope_key: "scope",

                generation: "gen-1",

                existing_owner: Some("run-1"),

                scope: RootAuthorityScope::Business,
            })
            .unwrap();
        // A re-authorization of the same root bumps the binding epoch: any driver bound
        // under the old epoch must re-bind, which is what makes a resume safe.
        assert_eq!(regranted.binding_epoch, 2);
    }

    #[test]
    fn foreground_root_lifecycle_revocation_issues_a_sequence_above_the_last_answer() {
        let mut table = RootAuthorityTable::new();
        table
            .authorize(&AuthorizeRequest {
                intent: &intent("op-1", "thread:a", "/tmp/a"),

                root_id: "root-1",

                client_run_id: "run-1",

                scope_key: "scope",

                generation: "gen-1",

                existing_owner: None,

                scope: RootAuthorityScope::Business,
            })
            .unwrap();
        let frame =
            table.answer_challenge("n1", "root-1", 4, "gen-1", Duration::from_secs(30), "d");
        match frame {
            BridgeFrame::Authority(AuthorityMessage::Renew { stamp }) => {
                assert_eq!(stamp.authority_seq, 4)
            }
            other => panic!("unexpected answer: {other:?}"),
        }
        let revoked = table.revoke_owner("run-1");
        assert_eq!(revoked, vec![("root-1".to_string(), 5)]);
        assert!(table.grant("root-1").is_none());
    }

    #[test]
    fn foreground_root_lifecycle_a_challenge_for_an_unknown_root_is_refused() {
        let mut table = RootAuthorityTable::new();
        let frame = answer(
            &mut table,
            &BridgeFrame::Challenge {
                nonce: "n1".into(),
                root_id: "root-9".into(),
                authority_seq: 1,
            },
            "gen-1",
            "daemon-1",
            Duration::from_secs(30),
        )
        .unwrap();
        assert_eq!(refusal("n1", AuthorityRefusal::NoOwner), frame);
    }

    #[test]
    fn foreground_root_lifecycle_an_answer_never_goes_below_the_asked_sequence() {
        let mut table = RootAuthorityTable::new();
        table
            .authorize(&AuthorizeRequest {
                intent: &intent("op-1", "thread:a", "/tmp/a"),

                root_id: "root-1",

                client_run_id: "run-1",

                scope_key: "scope",

                generation: "gen-1",

                existing_owner: None,

                scope: RootAuthorityScope::Business,
            })
            .unwrap();
        // A challenge that names a higher sequence than this guardian has issued wins:
        // the daemon knows about withdrawals this side may have missed.
        let frame =
            table.answer_challenge("n1", "root-1", 42, "gen-1", Duration::from_secs(30), "d");
        match frame {
            BridgeFrame::Authority(AuthorityMessage::Renew { stamp }) => {
                assert_eq!(stamp.authority_seq, 42)
            }
            other => panic!("unexpected answer: {other:?}"),
        }
    }

    #[test]
    fn foreground_root_lifecycle_reservations_are_keyed_by_operation() {
        let mut table = RootAuthorityTable::new();
        table
            .authorize(&AuthorizeRequest {
                intent: &intent("op-1", "thread:a", "/tmp/a"),

                root_id: "root-1",

                client_run_id: "run-1",

                scope_key: "scope",

                generation: "gen-1",

                existing_owner: None,

                scope: RootAuthorityScope::Business,
            })
            .unwrap();
        assert_eq!(table.reservation("op-1").as_deref(), Some("root-1"));
        assert_eq!(
            table.reservation_workspace("op-1"),
            Some(std::path::Path::new("/tmp/a"))
        );
        assert!(table.reservation("op-2").is_none());
    }

    #[test]
    fn foreground_root_lifecycle_the_bridge_never_answers_authority_echoes() {
        let mut table = RootAuthorityTable::new();
        let echoed = BridgeFrame::Authority(AuthorityMessage::Renew {
            stamp: AuthorityStamp {
                scope_key: "scope".into(),
                generation: "gen-1".into(),
                owner: "run-1".into(),
                root_id: "root-1".into(),
                binding_epoch: 1,
                authority_seq: 1,
                challenge_nonce: "n1".into(),
                remaining_lease_ms: 1000,
                authority_scope: RootAuthorityScope::Business,
            },
        });
        assert!(
            answer(
                &mut table,
                &echoed,
                "gen-1",
                "daemon-1",
                Duration::from_secs(30)
            )
            .is_none()
        );
    }
}
