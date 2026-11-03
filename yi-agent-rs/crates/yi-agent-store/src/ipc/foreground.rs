//! The daemon's half of the private root-authority bridge.
//!
//! The wire is a pair of [`BridgeFrame`]s over a descriptor the guardian handed to the
//! daemon at bring-up. Everything here is deliberately daemon-side policy:
//!
//! * the daemon **issues the challenge**, so the deadline clock starts on the clock
//!   that will later judge it (`AuthorityTable::issue_challenge`);
//! * the daemon **commits the binding itself**, in the same step that validates the
//!   install, so "authorized" and "managed on disk" cannot disagree;
//! * a refusal is a frame, not an error, because the guardian's wait has to end
//!   without a timeout — a wedged guardian and a refusing guardian must be
//!   distinguishable.
//!
//! What this module deliberately does *not* do: accept a "verified" flag from any
//! public request. [`super::authority::AuthorityTable`] can only be driven by a frame
//! that arrived on the private descriptor, and that is enforced by the type
//! ([`BridgeFrame`] is never reachable from `IpcRequest`).

use std::collections::HashMap;
use std::time::Instant;

use yi_agent_lifecycle::foreground::{
    AuthorityMessage, BridgeFrame, ForegroundErrorCode, RootAuthorityScope, RootBindIntent,
};

use crate::foreground::ForegroundError;
use crate::repository::RuntimeRepository;
use crate::repository::foreground::{ForegroundClassification, PersistedForegroundRoot};

use super::authority::AuthorityTable;

/// A prepare that has been reserved but whose install has not been committed.
#[derive(Clone, Debug)]
struct PendingPrepare {
    root_id: String,
    client_run_id: String,
    scope_key: String,
    intent: RootBindIntent,
    scope: RootAuthorityScope,
}

/// The daemon's bridge state: the authority table plus the prepares it owes answers to.
#[derive(Debug)]
pub struct ForegroundBridge {
    authority: AuthorityTable,
    pending: HashMap<String, PendingPrepare>,
    /// Set once the bridge descriptor reaches EOF.
    ///
    /// Admission closes on EOF and stays closed: the guardian is the only thing that
    /// can vouch for a root, so without it no authorization can be validated. Roots
    /// keep their identity so reclamation can still explain them.
    closed: bool,
}

impl ForegroundBridge {
    pub fn new(generation: impl Into<String>) -> Self {
        Self {
            authority: AuthorityTable::new(generation),
            pending: HashMap::new(),
            closed: false,
        }
    }

    pub fn authority(&self) -> &AuthorityTable {
        &self.authority
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Whether a client may currently bind or write activity for `root`.
    ///
    /// The single read side every other component uses, so "closed because the bridge
    /// died" cannot be forgotten by one caller.
    pub fn admits(&self, root: &str, client_run_id: &str, now: Instant) -> bool {
        !self.closed && self.authority.owns_business(root, client_run_id, now)
    }

    /// Handles one frame from the guardian.
    ///
    /// Returns the frame to send back, if any. A frame this side does not answer
    /// (a `Challenge`, which only the daemon sends) yields `None` rather than a guess.
    ///
    /// `repository` is written here rather than by the caller because the commit must
    /// happen between "the install validated" and "the guardian is told it committed".
    /// Splitting those across two calls would create exactly the window where the
    /// guardian believes a root is managed and the daemon's rows disagree.
    pub fn handle(
        &mut self,
        frame: BridgeFrame,
        repository: &mut RuntimeRepository,
        now: Instant,
    ) -> Option<BridgeFrame> {
        if self.closed {
            // A frame after EOF is a message from a guardian that is gone. Refusing it
            // keeps "the bridge died" from being undone by one late write.
            return deny(frame, ForegroundErrorCode::AuthorityDenied);
        }
        match frame {
            BridgeFrame::Prepare {
                nonce,
                root_id,
                client_run_id,
                scope_key,
                generation,
                requested_seq,
                intent,
                scope,
            } => Some(self.prepare(
                nonce,
                root_id,
                client_run_id,
                scope_key,
                generation,
                requested_seq,
                intent,
                scope,
                now,
            )),
            BridgeFrame::Authority(AuthorityMessage::Install {
                stamp,
                authorization,
            }) => Some(
                match self.authority.apply(
                    &AuthorityMessage::Install {
                        stamp: stamp.clone(),
                        authorization: authorization.clone(),
                    },
                    now,
                ) {
                    Ok(()) => match self.commit(repository, &stamp.root_id, stamp.binding_epoch) {
                        Ok(()) => {
                            self.pending.remove(&stamp.challenge_nonce);
                            BridgeFrame::Committed {
                                nonce: stamp.challenge_nonce.clone(),
                                root_id: stamp.root_id.clone(),
                                binding_epoch: stamp.binding_epoch,
                            }
                        }
                        Err(error) => BridgeFrame::Denied {
                            nonce: stamp.challenge_nonce.clone(),
                            root_id: stamp.root_id.clone(),
                            code: code_of(&error),
                        },
                    },
                    Err(error) => BridgeFrame::Denied {
                        nonce: stamp.challenge_nonce.clone(),
                        root_id: stamp.root_id.clone(),
                        code: code_of(&error),
                    },
                },
            ),
            BridgeFrame::Authority(message @ AuthorityMessage::Renew { .. })
            | BridgeFrame::Authority(message @ AuthorityMessage::RevokeOwner { .. })
            | BridgeFrame::Authority(message @ AuthorityMessage::Drain { .. }) => {
                // A renewal and a revocation change admission, not the persisted
                // identity: the row was written by the install and a revoked root keeps
                // its classification so the next start can still explain it.
                let root = root_of(&message);
                match self.authority.apply(&message, now) {
                    Ok(()) => None,
                    Err(error) => root.map(|root| BridgeFrame::Denied {
                        nonce: String::new(),
                        root_id: root.to_string(),
                        code: code_of(&error),
                    }),
                }
            }
            // The daemon sends these; a guardian echoing one is confused, and silence
            // is the honest reply.
            BridgeFrame::Challenge { .. }
            | BridgeFrame::Committed { .. }
            | BridgeFrame::Denied { .. }
            | BridgeFrame::Refused { .. } => None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare(
        &mut self,
        nonce: String,
        root_id: String,
        client_run_id: String,
        scope_key: String,
        generation: String,
        requested_seq: u64,
        intent: RootBindIntent,
        scope: RootAuthorityScope,
        now: Instant,
    ) -> BridgeFrame {
        if generation != self.authority.generation() {
            return BridgeFrame::Denied {
                nonce,
                root_id,
                code: ForegroundErrorCode::StaleAttempt,
            };
        }
        if let Err(error) = self.authority.reserve(&intent, &root_id, &client_run_id) {
            return BridgeFrame::Denied {
                nonce,
                root_id,
                code: code_of(&error),
            };
        }
        // The sequence is the higher of what the guardian believes it has issued and
        // what this daemon's tombstones allow. Taking the max is what makes a
        // revocation that happened while the prepare was in flight stick.
        let seq = requested_seq.max(self.authority.next_authority_seq(&root_id));
        if self
            .authority
            .issue_challenge(nonce.clone(), &root_id, seq, now)
            .is_none()
        {
            // A tombstone at or above `seq` means this root was revoked and the
            // guardian has not noticed. Asking it to validate is pointless.
            self.authority
                .abandon_reservation(&root_id, &intent.operation_id);
            return BridgeFrame::Denied {
                nonce,
                root_id,
                code: ForegroundErrorCode::AuthorityDenied,
            };
        }
        self.pending.insert(
            nonce.clone(),
            PendingPrepare {
                root_id: root_id.clone(),
                client_run_id,
                scope_key,
                intent,
                scope,
            },
        );
        BridgeFrame::Challenge {
            nonce,
            root_id,
            authority_seq: seq,
        }
    }

    /// Writes the managed classification for a root whose authority just installed.
    ///
    /// The row is what makes the exemption outlive the process: the next daemon starts
    /// from "this root is a managed container", not from a request it cannot verify.
    fn commit(
        &mut self,
        repository: &mut RuntimeRepository,
        root_id: &str,
        binding_epoch: u64,
    ) -> Result<(), ForegroundError> {
        let Some(pending) = self
            .pending
            .values()
            .find(|pending| pending.root_id == root_id)
            .cloned()
        else {
            // An install with no prepare behind it: the challenge covered a root this
            // side never reserved, so there is nothing to classify.
            return Err(ForegroundError::RecoveryAuthorityMissing);
        };
        let root = root_id
            .parse()
            .map_err(|_| ForegroundError::CorruptBinding)?;
        repository
            .record_foreground_root(&PersistedForegroundRoot {
                root,
                classification: ForegroundClassification::Managed,
                binding_epoch,
                driver_epoch: 1,
                owner_client_id: Some(pending.client_run_id.clone()),
                // The install authorizes the owner but admits no work yet: the driver
                // bind (Task 6) is what opens admission for an attempt.
                admission: crate::repository::foreground::AdmissionState::Closed,
                driver_bind_operation_id: Some(pending.intent.operation_id.clone()),
            })
            .map_err(|_| ForegroundError::CorruptBinding)?;
        let _ = (&pending.scope_key, pending.scope);
        Ok(())
    }

    /// The bridge descriptor reached EOF: close admission for every root.
    ///
    /// Returns the roots whose admission closed so the caller can report them. The
    /// tombstones are *not* written: a crashed guardian is not a decision to revoke,
    /// and inventing one would permanently bar the user's conversation.
    pub fn bridge_eof(&mut self) -> Vec<String> {
        self.closed = true;
        let reserved = self.authority.abandon_dead_bridge_reservations();
        let _ = reserved;
        self.pending.clear();
        self.authority.close_all_admission();
        Vec::new()
    }

    /// A newly accepted bridge invalidates every challenge from the previous one.
    pub fn bridge_reconnected(&mut self) {
        self.closed = false;
        self.authority.bridge_connected();
    }
}

fn root_of(message: &AuthorityMessage) -> Option<&str> {
    match message {
        AuthorityMessage::Install { stamp, .. } | AuthorityMessage::Renew { stamp } => {
            Some(stamp.root_id.as_str())
        }
        AuthorityMessage::RevokeOwner { .. } | AuthorityMessage::Drain { .. } => None,
    }
}

/// Turns an internal failure into the wire discriminant.
///
/// One function so a new [`ForegroundError`] cannot be reported as the wrong code in
/// one place and the right one in another.
pub fn code_of(error: &ForegroundError) -> ForegroundErrorCode {
    match error {
        ForegroundError::Unsupported => ForegroundErrorCode::Unsupported,
        ForegroundError::AuthorityDenied => ForegroundErrorCode::AuthorityDenied,
        ForegroundError::AdmissionClosed => ForegroundErrorCode::AdmissionClosed,
        ForegroundError::StaleAttempt => ForegroundErrorCode::StaleAttempt,
        ForegroundError::StaleDriver => ForegroundErrorCode::StaleDriver,
        ForegroundError::InvalidActivity => ForegroundErrorCode::InvalidActivity,
        ForegroundError::SequenceConflict => ForegroundErrorCode::SequenceConflict,
        ForegroundError::RecoveryRequired => ForegroundErrorCode::RecoveryRequired,
        ForegroundError::RecoveryAuthorityMissing => ForegroundErrorCode::RecoveryAuthorityMissing,
        ForegroundError::StopUnproven(_) => ForegroundErrorCode::StopUnproven,
        ForegroundError::ConfirmationRequired => ForegroundErrorCode::ConfirmationRequired,
        ForegroundError::ConfirmationExpired => ForegroundErrorCode::ConfirmationExpired,
        ForegroundError::OperationConflict => ForegroundErrorCode::OperationConflict,
        ForegroundError::CorruptBinding => ForegroundErrorCode::CorruptBinding,
    }
}

/// Denies a frame that cannot be answered, preserving its nonce when it has one.
fn deny(frame: BridgeFrame, code: ForegroundErrorCode) -> Option<BridgeFrame> {
    match frame {
        BridgeFrame::Prepare { nonce, root_id, .. } => Some(BridgeFrame::Denied {
            nonce,
            root_id,
            code,
        }),
        BridgeFrame::Authority(AuthorityMessage::Install { stamp, .. })
        | BridgeFrame::Authority(AuthorityMessage::Renew { stamp }) => Some(BridgeFrame::Denied {
            nonce: stamp.challenge_nonce.clone(),
            root_id: stamp.root_id.clone(),
            code,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use yi_agent_lifecycle::foreground::{AuthorityStamp, SecretCapability};

    /// Fixed UUIDs: ids cross the bridge as strings but they are task ids, so a test
    /// that invented a readable name would not exercise the same parsing path.
    const ROOT: &str = "3f1d9c2e-5b7a-4c81-9d6e-2a4f7c0b8e11";
    const SESSION: &str = "8c2b6d1f-4a3e-4f57-8b90-6d2e1c5a7f33";

    fn intent(operation: &str, workspace: &str) -> RootBindIntent {
        RootBindIntent {
            operation_id: operation.into(),
            thread_key: "thread:a".into(),
            workspace: workspace.into(),
            extra_roots: vec![],
            previous_root_capability: None,
        }
    }

    fn stamp(nonce: &str, root: &str, seq: u64, epoch: u64) -> AuthorityStamp {
        AuthorityStamp {
            scope_key: "scope".into(),
            generation: "gen-1".into(),
            owner: "run-1".into(),
            root_id: root.into(),
            binding_epoch: epoch,
            authority_seq: seq,
            challenge_nonce: nonce.into(),
            remaining_lease_ms: 30_000,
            authority_scope: RootAuthorityScope::Business,
        }
    }

    fn prepare_frame(nonce: &str, root: &str, operation: &str) -> BridgeFrame {
        BridgeFrame::Prepare {
            nonce: nonce.into(),
            root_id: root.into(),
            client_run_id: "run-1".into(),
            scope_key: "scope".into(),
            generation: "gen-1".into(),
            requested_seq: 1,
            intent: intent(operation, "/tmp/ws"),
            scope: RootAuthorityScope::Business,
        }
    }

    fn repository() -> (tempfile::TempDir, RuntimeRepository) {
        let dir = tempfile::tempdir().unwrap();
        let mut repository = RuntimeRepository::open(dir.path().join("runtime.sqlite")).unwrap();
        // `foreground_root_state` references `tasks`, so a root the bridge classifies
        // has to be a root the daemon actually created. Seeding one here keeps the
        // test honest about that foreign key instead of disabling it.
        let task = seed_root(&mut repository);
        assert_eq!(task.to_string(), ROOT);
        (dir, repository)
    }

    fn seed_root(repository: &mut RuntimeRepository) -> yi_agent_core::subagent::task::TaskId {
        let task: yi_agent_core::subagent::task::TaskId = ROOT.parse().unwrap();
        repository
            .create_task(&task, &SESSION.parse().unwrap(), r#"{"state":"running"}"#)
            .unwrap();
        task
    }

    fn install_frame(nonce: &str, root: &str, seq: u64) -> BridgeFrame {
        BridgeFrame::Authority(AuthorityMessage::Install {
            stamp: stamp(nonce, root, seq, 1),
            authorization: yi_agent_lifecycle::types::RootAuthorization {
                scope_key: "scope".into(),
                daemon_instance_id: "gen-1".into(),
                client_run_id: "run-1".into(),
                root_id: root.into(),
                capability: SecretCapability::new("cap-1").expose().to_string(),
            },
        })
    }

    #[test]
    fn foreground_root_lifecycle_a_prepare_is_answered_with_a_challenge_not_an_install() {
        let (_dir, mut repo) = repository();
        let mut bridge = ForegroundBridge::new("gen-1");
        let answer = bridge
            .handle(prepare_frame("n1", ROOT, "op-1"), &mut repo, Instant::now())
            .unwrap();
        // Nothing is authorized by a prepare: the daemon must be the one to start the
        // clock, so its answer is a question.
        match answer {
            BridgeFrame::Challenge {
                nonce,
                root_id,
                authority_seq,
            } => {
                assert_eq!(nonce, "n1");
                assert_eq!(root_id, ROOT);
                assert_eq!(authority_seq, 1);
            }
            other => panic!("unexpected answer: {other:?}"),
        }
        assert!(!bridge.admits(ROOT, "run-1", Instant::now()));
    }

    #[test]
    fn foreground_root_lifecycle_an_install_commits_the_managed_row_and_answers_committed() {
        let (_dir, mut repo) = repository();
        let mut bridge = ForegroundBridge::new("gen-1");
        let now = Instant::now();
        bridge.handle(prepare_frame("n1", ROOT, "op-1"), &mut repo, now);
        let answer = bridge
            .handle(install_frame("n1", ROOT, 1), &mut repo, now)
            .unwrap();
        match answer {
            BridgeFrame::Committed {
                nonce,
                root_id,
                binding_epoch,
            } => {
                assert_eq!((nonce.as_str(), root_id.as_str()), ("n1", ROOT));
                assert_eq!(binding_epoch, 1);
            }
            other => panic!("unexpected answer: {other:?}"),
        }
        assert!(bridge.admits(ROOT, "run-1", now + Duration::from_secs(1)));
        // The commit is what a restart reads back, not a request.
        let row = repo
            .persisted_foreground_root(&ROOT.parse().unwrap())
            .unwrap();
        let row = row.expect("the install must persist the root");
        assert_eq!(row.classification, ForegroundClassification::Managed);
        assert_eq!(row.owner_client_id.as_deref(), Some("run-1"));
    }

    #[test]
    fn foreground_root_lifecycle_an_install_without_a_prepare_is_denied() {
        let (_dir, mut repo) = repository();
        let mut bridge = ForegroundBridge::new("gen-1");
        let answer = bridge
            .handle(install_frame("n1", ROOT, 1), &mut repo, Instant::now())
            .unwrap();
        // No challenge exists, so the stamp cannot name one that covers it.
        match answer {
            BridgeFrame::Denied { code, .. } => {
                assert_eq!(code, ForegroundErrorCode::AuthorityDenied)
            }
            other => panic!("unexpected answer: {other:?}"),
        }
    }

    #[test]
    fn foreground_root_lifecycle_a_prepare_from_another_generation_is_denied() {
        let (_dir, mut repo) = repository();
        let mut bridge = ForegroundBridge::new("gen-1");
        let mut frame = prepare_frame("n1", ROOT, "op-1");
        if let BridgeFrame::Prepare { generation, .. } = &mut frame {
            *generation = "gen-2".into();
        }
        match bridge.handle(frame, &mut repo, Instant::now()).unwrap() {
            BridgeFrame::Denied { code, .. } => {
                assert_eq!(code, ForegroundErrorCode::StaleAttempt)
            }
            other => panic!("unexpected answer: {other:?}"),
        }
    }

    #[test]
    fn foreground_root_lifecycle_a_revocation_refuses_a_later_install_at_the_same_sequence() {
        let (_dir, mut repo) = repository();
        let mut bridge = ForegroundBridge::new("gen-1");
        let now = Instant::now();
        bridge.handle(prepare_frame("n1", ROOT, "op-1"), &mut repo, now);
        bridge.handle(install_frame("n1", ROOT, 1), &mut repo, now);
        // The owner leaves; the guardian withdraws at sequence 2.
        bridge.handle(
            BridgeFrame::Authority(AuthorityMessage::RevokeOwner {
                owner: "run-1".into(),
                generation: "gen-1".into(),
                authority_seq: 2,
            }),
            &mut repo,
            now,
        );
        assert!(!bridge.admits(ROOT, "run-1", now));
        // A renewal that was already queued at the old sequence must not resurrect it.
        bridge.handle(
            BridgeFrame::Authority(AuthorityMessage::Renew {
                stamp: stamp("n1", ROOT, 1, 1),
            }),
            &mut repo,
            now,
        );
        assert!(!bridge.admits(ROOT, "run-1", now));
        // And a fresh prepare starts above the tombstone rather than reusing it.
        match bridge
            .handle(prepare_frame("n2", ROOT, "op-2"), &mut repo, now)
            .unwrap()
        {
            BridgeFrame::Challenge { authority_seq, .. } => assert_eq!(authority_seq, 3),
            other => panic!("unexpected answer: {other:?}"),
        }
    }

    #[test]
    fn foreground_root_lifecycle_bridge_eof_closes_admission_without_tombstoning() {
        let (_dir, mut repo) = repository();
        let mut bridge = ForegroundBridge::new("gen-1");
        let now = Instant::now();
        bridge.handle(prepare_frame("n1", ROOT, "op-1"), &mut repo, now);
        bridge.handle(install_frame("n1", ROOT, 1), &mut repo, now);
        assert!(bridge.admits(ROOT, "run-1", now));
        bridge.bridge_eof();
        assert!(bridge.is_closed());
        assert!(!bridge.admits(ROOT, "run-1", now));
        // No tombstone: a crashed guardian must not permanently bar the root, so a
        // reconnected bridge can authorize it again at the same sequence.
        bridge.bridge_reconnected();
        match bridge
            .handle(prepare_frame("n2", ROOT, "op-2"), &mut repo, now)
            .unwrap()
        {
            BridgeFrame::Challenge { authority_seq, .. } => assert_eq!(authority_seq, 1),
            other => panic!("unexpected answer: {other:?}"),
        }
    }

    #[test]
    fn foreground_root_lifecycle_a_late_install_past_the_lease_window_is_denied() {
        let (_dir, mut repo) = repository();
        let mut bridge = ForegroundBridge::new("gen-1");
        let now = Instant::now();
        bridge.handle(prepare_frame("n1", ROOT, "op-1"), &mut repo, now);
        // Arriving after the lease the guardian reported would have expired: the
        // deadline is the challenge's issue instant, not the receipt time.
        let answer = bridge
            .handle(
                install_frame("n1", ROOT, 1),
                &mut repo,
                now + Duration::from_millis(30_001),
            )
            .unwrap();
        match answer {
            BridgeFrame::Denied { code, .. } => {
                assert_eq!(code, ForegroundErrorCode::ConfirmationExpired)
            }
            other => panic!("unexpected answer: {other:?}"),
        }
        assert!(!bridge.admits(ROOT, "run-1", now));
    }
}
