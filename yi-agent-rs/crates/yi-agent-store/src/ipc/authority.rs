//! The daemon's private, verified root-authority table.
//!
//! Authority to bind a foreground root reaches the daemon over a descriptor the
//! guardian handed it at bring-up, never over the business socket. That asymmetry is
//! the security property: a tool process (which holds a *spawn* channel, and whose
//! requests the guardian refuses to treat as authority) cannot install authority for
//! itself, and a client cannot set a "verified" flag in JSON because no JSON reader
//! can produce [`VerifiedAuthority`].
//!
//! Three things are enforced here rather than at the call sites that use them:
//!
//! * **Liveness is a lease, not a message.** An authorization is only as good as the
//!   remaining part of the lease the guardian validated. The daemon records the local
//!   monotonic instant when it *issued the challenge*, so a frame that sat in a queue
//!   cannot extend its own deadline by arriving late.
//! * **Sequence is monotonic per bridge, tombstones included.** A release writes a
//!   tombstone; any later `Install`/`Renew` at or below it is refused forever, so a
//!   message that was already in flight when the client left cannot resurrect it.
//! * **Nothing here is derivable from a thread key.** The table is keyed by root, and
//!   the capability it hands out is generated, not computed.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use yi_agent_lifecycle::foreground::{
    AuthorityMessage, AuthorityStamp, RootAuthorityScope, RootBindIntent,
};
use yi_agent_lifecycle::types::RootAuthorization;

use crate::foreground::ForegroundError;

/// How long a challenge issued by the daemon stays answerable.
///
/// This is *not* the lease window: it only bounds the round trip "daemon asks, the
/// guardian answers". A guardian that takes longer than this is either wedged or
/// replaying something old, and either way its answer must not be honoured.
pub const CHALLENGE_TTL: Duration = Duration::from_secs(5);

/// One challenge the daemon issued and has not seen answered.
#[derive(Clone, Debug)]
struct Challenge {
    root_id: String,
    /// The local instant the challenge was created.
    ///
    /// The deadline is `issued_at + remaining_lease_ms`, computed on this clock. Using
    /// the receipt time instead would let a delayed frame renew a lease that had
    /// already run out on the guardian's side.
    issued_at: Instant,
    /// The bridge this challenge was issued on. An answer on a different bridge is a
    /// different generation's business, even if the nonce matched.
    bridge: u64,
}

/// Authority the daemon will accept for one root.
#[derive(Clone, Debug)]
struct InstalledAuthority {
    client_run_id: String,
    /// The scope key the guardian validated this root under.
    ///
    /// Kept so the install's commit can be checked against the *prepare* that reserved
    /// the root: without it, a reservation made for one scope could be completed by an
    /// install that validated a different one, and the two sides would disagree about
    /// which project the authority belongs to.
    scope_key: String,
    binding_epoch: u64,
    authority_seq: u64,
    scope: RootAuthorityScope,
    /// SHA-256 of the capability the guardian issued for this root.
    ///
    /// Only the digest is kept: the table has to *recognise* the capability a caller
    /// presents, never be able to reproduce it. Without this, any process that could
    /// guess a root id and an owner name could ask the daemon to bind a driver for a
    /// root it does not own.
    capability_digest: [u8; 32],
    /// The last moment this authority may be used, on the daemon's monotonic clock.
    expires_at: Instant,
}

/// A root whose owner was revoked, remembered so a late message cannot resurrect it.
///
/// Only the sequence is *read*: a tombstone is a bar, not a record of who set it, and
/// the generation that wrote it is deliberately not consulted -- a revocation must keep
/// holding across a generation change, which is the whole point of remembering it.
#[derive(Clone, Debug)]
struct Tombstone {
    authority_seq: u64,
}

/// One root's reservation, made by a prepare that has not completed.
///
/// A reservation holds no execution and grants no business: it exists only so the
/// guardian and the daemon agree on *which* root an authorization is about while the
/// two-phase install runs.
#[derive(Clone, Debug)]
struct Reservation {
    operation_id: String,
    thread_key: String,
    workspace: std::path::PathBuf,
    extra_roots: Vec<std::path::PathBuf>,
    client_run_id: String,
    bridge: u64,
}

/// The daemon's view of installed foreground authority.
///
/// Plain state, mutated through the coordinator's existing serialized entry point. It
/// deliberately owns no lock of its own: two locks around "check the authority, then
/// commit the binding" is how such a check stops being atomic.
#[derive(Debug, Default)]
pub struct AuthorityTable {
    /// The daemon generation these entries are valid in.
    generation: String,
    challenges: HashMap<String, Challenge>,
    authorities: HashMap<String, InstalledAuthority>,
    tombstones: HashMap<String, Tombstone>,
    reservations: HashMap<String, Reservation>,
    /// Monotonic counter for issued nonces. Not a secret; it only has to be unique.
    nonce_seq: u64,
    /// Identifies the currently connected bridge. Bumped on every (re)connect so a
    /// challenge from a previous bridge can never be answered on the new one.
    bridge: u64,
}

impl AuthorityTable {
    pub fn new(generation: impl Into<String>) -> Self {
        Self {
            generation: generation.into(),
            ..Self::default()
        }
    }

    pub fn generation(&self) -> &str {
        &self.generation
    }

    /// Called when a private bridge connection is accepted.
    ///
    /// Every outstanding challenge is dropped: they were issued to a peer that is no
    /// longer connected, and honouring one would mean honouring an answer whose
    /// question nobody is waiting for.
    pub fn bridge_connected(&mut self) -> u64 {
        self.bridge = self.bridge.wrapping_add(1);
        self.challenges.clear();
        self.bridge
    }

    /// The bridge that is currently connected, if any.
    pub fn bridge(&self) -> u64 {
        self.bridge
    }

    /// Issues a challenge for one nonce.
    ///
    /// Returns `None` when the root already has a tombstone at or above `authority_seq`
    /// — there is no point asking the guardian to validate authority the daemon has
    /// already been told to revoke.
    pub fn issue_challenge(
        &mut self,
        nonce: impl Into<String>,
        root_id: &str,
        authority_seq: u64,
        now: Instant,
    ) -> Option<()> {
        if self
            .tombstones
            .get(root_id)
            .is_some_and(|tombstone| authority_seq <= tombstone.authority_seq)
        {
            return None;
        }
        self.nonce_seq = self.nonce_seq.wrapping_add(1);
        self.challenges.insert(
            nonce.into(),
            Challenge {
                root_id: root_id.to_string(),
                issued_at: now,
                bridge: self.bridge,
            },
        );
        Some(())
    }

    /// Validates one authority message, then installs, renews or revokes.
    ///
    /// The order is fixed: generation, then tombstone, then challenge, then deadline.
    /// Checking the deadline last is what makes the bound meaningful — a message whose
    /// lease had already run out cannot be rescued by a fresh challenge.
    pub fn apply(
        &mut self,
        message: &AuthorityMessage,
        now: Instant,
    ) -> Result<(), ForegroundError> {
        match message {
            AuthorityMessage::Install {
                stamp,
                authorization,
            } => {
                self.check_stamp(stamp, now)?;
                self.check_authorization_matches_stamp(stamp, authorization)?;
                self.consume_challenge(stamp, now)?;
                self.authorities.insert(
                    stamp.root_id.clone(),
                    InstalledAuthority {
                        client_run_id: stamp.owner.clone(),
                        scope_key: authorization.scope_key.clone(),
                        binding_epoch: stamp.binding_epoch,
                        authority_seq: stamp.authority_seq,
                        scope: stamp.authority_scope,
                        capability_digest: digest_of_capability(&authorization.capability),
                        expires_at: now + Duration::from_millis(stamp.remaining_lease_ms),
                    },
                );
                // Once a root is installed, the reservation that led here is spent: an
                // operation id must not be reusable to authorize a second time.
                self.reservations
                    .retain(|_, reservation| reservation.operation_id != stamp.root_id);
                Ok(())
            }
            AuthorityMessage::Renew { stamp } => {
                self.check_stamp(stamp, now)?;
                self.consume_challenge(stamp, now)?;
                let authority = self
                    .authorities
                    .get_mut(&stamp.root_id)
                    .ok_or(ForegroundError::RecoveryAuthorityMissing)?;
                if authority.client_run_id != stamp.owner
                    || authority.binding_epoch != stamp.binding_epoch
                {
                    // A renewal for a different owner or binding epoch is not a
                    // renewal: installing it would silently transfer the root.
                    return Err(ForegroundError::StaleAttempt);
                }
                authority.authority_seq = stamp.authority_seq;
                authority.expires_at = now + Duration::from_millis(stamp.remaining_lease_ms);
                Ok(())
            }
            AuthorityMessage::RevokeOwner {
                owner,
                generation,
                authority_seq,
            } => {
                if generation != &self.generation {
                    // A revocation from another generation names roots this daemon
                    // never authorized; applying it would revoke nothing and hide that
                    // fact behind an `Ok`.
                    return Err(ForegroundError::StaleDriver);
                }
                let roots: Vec<String> = self
                    .authorities
                    .iter()
                    .filter(|(_, authority)| &authority.client_run_id == owner)
                    .map(|(root, _)| root.clone())
                    .collect();
                for root in roots {
                    self.authorities.remove(&root);
                    self.record_tombstone(&root, *authority_seq, generation);
                }
                Ok(())
            }
            AuthorityMessage::Drain {
                generation,
                authority_seq,
            } => {
                if generation != &self.generation {
                    return Err(ForegroundError::StaleDriver);
                }
                let roots: Vec<String> = self.authorities.keys().cloned().collect();
                for root in roots {
                    self.authorities.remove(&root);
                    self.record_tombstone(&root, *authority_seq, generation);
                }
                self.challenges.clear();
                Ok(())
            }
        }
    }

    fn check_stamp(&self, stamp: &AuthorityStamp, _now: Instant) -> Result<(), ForegroundError> {
        if stamp.generation != self.generation {
            return Err(ForegroundError::StaleAttempt);
        }
        // An authority sequence at or below a tombstone is a revoked generation's
        // message -- the reason tombstones outlive the entries they killed.
        if self
            .tombstones
            .get(&stamp.root_id)
            .is_some_and(|tombstone| stamp.authority_seq <= tombstone.authority_seq)
        {
            return Err(ForegroundError::AuthorityDenied);
        }
        Ok(())
    }

    fn check_authorization_matches_stamp(
        &self,
        stamp: &AuthorityStamp,
        authorization: &RootAuthorization,
    ) -> Result<(), ForegroundError> {
        // Every field that decides *whose* root this is must agree. A stamp and an
        // authorization that disagree would install one owner's authority under
        // another's root.
        if authorization.root_id != stamp.root_id
            || authorization.client_run_id != stamp.owner
            || authorization.scope_key != stamp.scope_key
        {
            return Err(ForegroundError::AuthorityDenied);
        }
        Ok(())
    }

    fn consume_challenge(
        &mut self,
        stamp: &AuthorityStamp,
        now: Instant,
    ) -> Result<(), ForegroundError> {
        let challenge = self
            .challenges
            .remove(&stamp.challenge_nonce)
            .ok_or(ForegroundError::AuthorityDenied)?;
        if challenge.root_id != stamp.root_id || challenge.bridge != self.bridge {
            return Err(ForegroundError::AuthorityDenied);
        }
        // The conservative deadline: the daemon's own issue instant plus the lease
        // the guardian saw *then*. A frame that waited in a queue fails here.
        let deadline = challenge.issued_at + Duration::from_millis(stamp.remaining_lease_ms);
        if now >= deadline {
            return Err(ForegroundError::ConfirmationExpired);
        }
        Ok(())
    }

    fn record_tombstone(&mut self, root: &str, authority_seq: u64, _generation: &str) {
        let entry = self
            .tombstones
            .entry(root.to_string())
            .or_insert(Tombstone { authority_seq });
        // Monotonic: a second revocation must not lower the bar for a late message.
        if authority_seq > entry.authority_seq {
            entry.authority_seq = authority_seq;
        }
    }

    /// The installed authority for one root, if it is still live.
    pub fn live_authority(&self, root: &str, now: Instant) -> Option<RootAuthorityScope> {
        self.authorities
            .get(root)
            .filter(|authority| authority.expires_at > now)
            .map(|authority| authority.scope)
    }

    /// Whether the given client currently owns this root with live business authority.
    pub fn owns_business(&self, root: &str, client_run_id: &str, now: Instant) -> bool {
        self.live_authority(root, now) == Some(RootAuthorityScope::Business)
            && self
                .authorities
                .get(root)
                .is_some_and(|authority| authority.client_run_id == client_run_id)
    }

    /// The binding epoch the guardian assigned, for the binding commit to record.
    /// The owner, scope key, binding epoch and authority sequence of a live install.
    ///
    /// One accessor rather than four so a caller that has to agree with the table checks
    /// a single snapshot of it, not four reads that a concurrent renew could straddle.
    pub fn installed_identity(&self, root: &str) -> Option<(String, String, u64, u64)> {
        self.authorities.get(root).map(|authority| {
            (
                authority.client_run_id.clone(),
                authority.scope_key.clone(),
                authority.binding_epoch,
                authority.authority_seq,
            )
        })
    }

    pub fn binding_epoch(&self, root: &str) -> Option<u64> {
        self.authorities
            .get(root)
            .map(|authority| authority.binding_epoch)
    }

    pub fn tombstone_seq(&self, root: &str) -> Option<u64> {
        self.tombstones.get(root).map(|entry| entry.authority_seq)
    }

    /// The sequence number to use for the next challenge about `root`.
    ///
    /// One above the tombstone so a challenge can never be issued for a sequence the
    /// daemon would immediately refuse.
    pub fn next_authority_seq(&self, root: &str) -> u64 {
        self.tombstone_seq(root)
            .map_or(1, |seq| seq.saturating_add(1))
    }

    /// Whether `capability` is the one the guardian issued for this root and owner.
    ///
    /// This is the daemon's half of "a client cannot set a verified flag": knowing the
    /// root id and the owner name is not authority, so the presented secret has to
    /// match the digest of the one that arrived over the private bridge.
    pub fn presents_capability(
        &self,
        root: &str,
        client_run_id: &str,
        capability: &str,
        now: Instant,
    ) -> bool {
        self.authorities.get(root).is_some_and(|authority| {
            authority.expires_at > now
                && authority.scope == RootAuthorityScope::Business
                && authority.client_run_id == client_run_id
                && constant_time_eq(
                    &authority.capability_digest,
                    &digest_of_capability(capability),
                )
        })
    }

    /// Records a reservation made by a prepare that has not completed.
    ///
    /// A reservation grants nothing. It exists so the same operation id is idempotent
    /// and a different intent under the same id is refused rather than silently
    /// re-pointed at a new workspace.
    pub fn reserve(
        &mut self,
        intent: &RootBindIntent,
        root_id: &str,
        client_run_id: &str,
    ) -> Result<(), ForegroundError> {
        if let Some(existing) = self.reservations.get(root_id) {
            if existing.operation_id == intent.operation_id
                && existing.thread_key == intent.thread_key
                && existing.workspace == intent.workspace
                && existing.extra_roots == intent.extra_roots
                && existing.client_run_id == client_run_id
            {
                // Same operation, same intent: idempotent.
                return Ok(());
            }
            if existing.operation_id == intent.operation_id {
                return Err(ForegroundError::OperationConflict);
            }
        }
        self.reservations.insert(
            root_id.to_string(),
            Reservation {
                operation_id: intent.operation_id.clone(),
                thread_key: intent.thread_key.clone(),
                workspace: intent.workspace.clone(),
                extra_roots: intent.extra_roots.clone(),
                client_run_id: client_run_id.to_string(),
                bridge: self.bridge,
            },
        );
        Ok(())
    }

    pub fn reservation_owner(&self, root: &str) -> Option<&str> {
        self.reservations
            .get(root)
            .map(|reservation| reservation.client_run_id.as_str())
    }

    /// Whether the reservation for `root` still describes exactly this intent.
    ///
    /// The install carries only the stamp and the authorization; neither names the
    /// workspace or the thread key. Without this check the commit could classify a root
    /// under an operation whose intent was about a different conversation, which is the
    /// one thing a reservation exists to prevent.
    pub fn reservation_matches(&self, root: &str, intent: &RootBindIntent) -> bool {
        self.reservations.get(root).is_some_and(|reservation| {
            reservation.operation_id == intent.operation_id
                && reservation.thread_key == intent.thread_key
                && reservation.workspace == intent.workspace
                && reservation.extra_roots == intent.extra_roots
        })
    }

    /// Drops a reservation that failed to install.
    ///
    /// Idempotent and safe to call for a reservation that is already gone: a failed
    /// prepare that retries must not find a stale reservation blocking it.
    pub fn abandon_reservation(&mut self, root: &str, operation_id: &str) -> bool {
        let Some(reservation) = self.reservations.get(root) else {
            return false;
        };
        if reservation.operation_id != operation_id {
            return false;
        }
        self.reservations.remove(root);
        true
    }

    /// Drops reservations whose bridge is gone.
    ///
    /// A bridge that disconnected cannot complete its install, so anything it
    /// reserved is abandoned. Done on *disconnect* rather than on a timer because
    /// there is no timer that knows when the guardian died.
    pub fn abandon_dead_bridge_reservations(&mut self) -> usize {
        let bridge = self.bridge;
        let before = self.reservations.len();
        self.reservations
            .retain(|_, reservation| reservation.bridge == bridge);
        before - self.reservations.len()
    }

    /// Every root this table currently authorizes, for the shutdown/drain report.
    pub fn authorized_roots(&self) -> Vec<String> {
        self.authorities.keys().cloned().collect()
    }

    /// Closes admission for every root without recording tombstones.
    ///
    /// Used when the bridge reaches EOF: the guardian is gone, so no new business may
    /// start, but the roots keep their *identity* so reclamation can still explain
    /// them. That is the difference between "closed" and "revoked": a revocation is a
    /// decision the guardian made, and inventing one here would let a crashed guardian
    /// permanently bar a user's conversation.
    pub fn close_all_admission(&mut self) {
        self.authorities.clear();
        self.challenges.clear();
    }
}

/// SHA-256 of a capability, as the table stores it.
fn digest_of_capability(capability: &str) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(capability.as_bytes());
    hasher.finalize().into()
}

/// Compares two digests without an early exit on the first differing byte.
///
/// The digests are not secrets, but the comparison decides an admission, and a
/// timing signal that reveals how many leading bytes matched would let a caller
/// search for a capability digest byte by byte.
fn constant_time_eq(left: &[u8; 32], right: &[u8; 32]) -> bool {
    let mut difference = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        difference |= a ^ b;
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use yi_agent_lifecycle::foreground::AuthorityStamp;

    const GENERATION: &str = "gen-1";
    const ROOT: &str = "root-1";
    const OWNER: &str = "run-1";

    fn table() -> AuthorityTable {
        let mut table = AuthorityTable::new(GENERATION);
        table.bridge_connected();
        table
    }

    fn stamp(nonce: &str, seq: u64, remaining_ms: u64) -> AuthorityStamp {
        AuthorityStamp {
            scope_key: "scope".into(),
            generation: GENERATION.into(),
            owner: OWNER.into(),
            root_id: ROOT.into(),
            binding_epoch: 1,
            authority_seq: seq,
            challenge_nonce: nonce.into(),
            remaining_lease_ms: remaining_ms,
            authority_scope: RootAuthorityScope::Business,
        }
    }

    fn authorization(root: &str, owner: &str, scope: &str) -> RootAuthorization {
        RootAuthorization {
            scope_key: scope.into(),
            daemon_instance_id: "daemon-1".into(),
            client_run_id: owner.into(),
            root_id: root.into(),
            capability: "cap".into(),
        }
    }

    fn install(
        table: &mut AuthorityTable,
        nonce: &str,
        seq: u64,
        now: Instant,
    ) -> Result<(), ForegroundError> {
        table.issue_challenge(nonce, ROOT, seq, now).unwrap();
        table.apply(
            &AuthorityMessage::Install {
                stamp: stamp(nonce, seq, 30_000),
                authorization: authorization(ROOT, OWNER, "scope"),
            },
            now,
        )
    }

    #[test]
    fn foreground_root_lifecycle_business_install_then_renew_is_live() {
        let now = Instant::now();
        let mut table = table();
        install(&mut table, "n1", 1, now).unwrap();
        assert!(table.owns_business(ROOT, OWNER, now));
        assert_eq!(table.binding_epoch(ROOT), Some(1));

        table.issue_challenge("n2", ROOT, 2, now).unwrap();
        table
            .apply(
                &AuthorityMessage::Renew {
                    stamp: stamp("n2", 2, 30_000),
                },
                now,
            )
            .unwrap();
        assert!(table.owns_business(ROOT, OWNER, now));
    }

    #[test]
    fn foreground_root_lifecycle_an_unknown_nonce_is_refused() {
        let now = Instant::now();
        let mut table = table();
        // No challenge issued: the frame is either replayed or from another bridge.
        let error = table
            .apply(
                &AuthorityMessage::Install {
                    stamp: stamp("never-issued", 1, 30_000),
                    authorization: authorization(ROOT, OWNER, "scope"),
                },
                now,
            )
            .unwrap_err();
        assert_eq!(error, ForegroundError::AuthorityDenied);
        assert!(!table.owns_business(ROOT, OWNER, now));
    }

    #[test]
    fn foreground_root_lifecycle_a_nonce_cannot_be_reused() {
        let now = Instant::now();
        let mut table = table();
        install(&mut table, "n1", 1, now).unwrap();
        let error = table
            .apply(
                &AuthorityMessage::Install {
                    stamp: stamp("n1", 2, 30_000),
                    authorization: authorization(ROOT, OWNER, "scope"),
                },
                now,
            )
            .unwrap_err();
        assert_eq!(error, ForegroundError::AuthorityDenied);
    }

    #[test]
    fn foreground_root_lifecycle_a_delayed_install_is_refused_past_its_lease() {
        // The whole reason the daemon timestamps the challenge rather than the receipt.
        let issued = Instant::now();
        let mut table = table();
        table.issue_challenge("n1", ROOT, 1, issued).unwrap();
        let late = issued + Duration::from_millis(31_000);
        let error = table
            .apply(
                &AuthorityMessage::Install {
                    stamp: stamp("n1", 1, 30_000),
                    authorization: authorization(ROOT, OWNER, "scope"),
                },
                late,
            )
            .unwrap_err();
        assert_eq!(error, ForegroundError::ConfirmationExpired);
        assert!(!table.owns_business(ROOT, OWNER, late));
    }

    #[test]
    fn foreground_root_lifecycle_a_stamp_for_another_generation_is_refused() {
        let now = Instant::now();
        let mut table = table();
        table.issue_challenge("n1", ROOT, 1, now).unwrap();
        let mut wrong = stamp("n1", 1, 30_000);
        wrong.generation = "gen-2".into();
        let error = table
            .apply(
                &AuthorityMessage::Install {
                    stamp: wrong,
                    authorization: authorization(ROOT, OWNER, "scope"),
                },
                now,
            )
            .unwrap_err();
        assert_eq!(error, ForegroundError::StaleAttempt);
    }

    #[test]
    fn foreground_root_lifecycle_an_authorization_for_another_root_is_refused() {
        let now = Instant::now();
        let mut table = table();
        table.issue_challenge("n1", ROOT, 1, now).unwrap();
        let error = table
            .apply(
                &AuthorityMessage::Install {
                    stamp: stamp("n1", 1, 30_000),
                    authorization: authorization("root-2", OWNER, "scope"),
                },
                now,
            )
            .unwrap_err();
        assert_eq!(error, ForegroundError::AuthorityDenied);
    }

    #[test]
    fn foreground_root_lifecycle_revocation_tombstones_late_installs() {
        let now = Instant::now();
        let mut table = table();
        install(&mut table, "n1", 5, now).unwrap();
        assert!(table.owns_business(ROOT, OWNER, now));

        table
            .apply(
                &AuthorityMessage::RevokeOwner {
                    owner: OWNER.into(),
                    generation: GENERATION.into(),
                    authority_seq: 6,
                },
                now,
            )
            .unwrap();
        assert!(!table.owns_business(ROOT, OWNER, now));

        // A message that was already in flight when the client left must not revive it.
        let error = table
            .apply(
                &AuthorityMessage::Install {
                    stamp: stamp("n-old", 6, 30_000),
                    authorization: authorization(ROOT, OWNER, "scope"),
                },
                now,
            )
            .unwrap_err();
        assert_eq!(error, ForegroundError::AuthorityDenied);
        // Nor may a renewal at the same sequence resurrect it.
        let error = table
            .apply(
                &AuthorityMessage::Renew {
                    stamp: stamp("n-old", 6, 30_000),
                },
                now,
            )
            .unwrap_err();
        assert_eq!(error, ForegroundError::AuthorityDenied);
    }

    #[test]
    fn foreground_root_lifecycle_a_challenge_is_not_issued_below_a_tombstone() {
        let now = Instant::now();
        let mut table = table();
        install(&mut table, "n1", 5, now).unwrap();
        table
            .apply(
                &AuthorityMessage::RevokeOwner {
                    owner: OWNER.into(),
                    generation: GENERATION.into(),
                    authority_seq: 9,
                },
                now,
            )
            .unwrap();
        assert!(table.issue_challenge("n2", ROOT, 9, now).is_none());
        assert_eq!(table.next_authority_seq(ROOT), 10);
    }

    #[test]
    fn foreground_root_lifecycle_bridge_eof_closes_without_tombstoning() {
        // A crashed guardian must not permanently bar the user's conversation: it
        // closes admission now, and a fresh authorization may reopen it later.
        let now = Instant::now();
        let mut table = table();
        install(&mut table, "n1", 1, now).unwrap();
        table.close_all_admission();
        assert!(!table.owns_business(ROOT, OWNER, now));
        assert_eq!(
            table.tombstone_seq(ROOT),
            None,
            "an EOF is not a revocation decision"
        );
    }

    #[test]
    fn foreground_root_lifecycle_a_renewal_cannot_transfer_ownership() {
        let now = Instant::now();
        let mut table = table();
        install(&mut table, "n1", 1, now).unwrap();
        table.issue_challenge("n2", ROOT, 2, now).unwrap();
        let mut other = stamp("n2", 2, 30_000);
        other.owner = "run-2".into();
        let error = table
            .apply(&AuthorityMessage::Renew { stamp: other }, now)
            .unwrap_err();
        assert_eq!(error, ForegroundError::StaleAttempt);
        assert!(table.owns_business(ROOT, OWNER, now));
        assert!(!table.owns_business(ROOT, "run-2", now));
    }

    #[test]
    fn foreground_root_lifecycle_expired_authority_is_not_live() {
        let now = Instant::now();
        let mut table = table();
        install(&mut table, "n1", 1, now).unwrap();
        let later = now + Duration::from_millis(30_001);
        assert!(!table.owns_business(ROOT, OWNER, later));
    }

    #[test]
    fn foreground_root_lifecycle_reservations_are_idempotent_per_operation() {
        let mut table = table();
        let intent = RootBindIntent {
            operation_id: "op-1".into(),
            thread_key: "thread:a".into(),
            workspace: "/tmp/a".into(),
            extra_roots: vec![],
            previous_root_capability: None,
        };
        table.reserve(&intent, ROOT, OWNER).unwrap();
        // Same operation and intent: idempotent, not a conflict.
        table.reserve(&intent, ROOT, OWNER).unwrap();
        assert_eq!(table.reservation_owner(ROOT), Some(OWNER));

        let mut different = intent.clone();
        different.workspace = "/tmp/b".into();
        assert_eq!(
            table.reserve(&different, ROOT, OWNER),
            Err(ForegroundError::OperationConflict)
        );
        assert!(!table.abandon_reservation(ROOT, "op-2"));
        assert!(table.abandon_reservation(ROOT, "op-1"));
        assert!(table.reservation_owner(ROOT).is_none());
    }

    #[test]
    fn foreground_root_lifecycle_a_new_bridge_orphans_old_challenges() {
        let now = Instant::now();
        let mut table = table();
        table.issue_challenge("n1", ROOT, 1, now).unwrap();
        // The guardian reconnected: the answer it now sends belongs to a question
        // asked of a peer that is gone.
        table.bridge_connected();
        let error = table
            .apply(
                &AuthorityMessage::Install {
                    stamp: stamp("n1", 1, 30_000),
                    authorization: authorization(ROOT, OWNER, "scope"),
                },
                now,
            )
            .unwrap_err();
        assert_eq!(error, ForegroundError::AuthorityDenied);
    }

    #[test]
    fn foreground_root_lifecycle_a_drain_tombstones_every_authorized_root() {
        let now = Instant::now();
        let mut table = table();
        install(&mut table, "n1", 1, now).unwrap();
        table
            .apply(
                &AuthorityMessage::Drain {
                    generation: GENERATION.into(),
                    authority_seq: 5,
                },
                now,
            )
            .unwrap();
        assert!(table.authorized_roots().is_empty());
        assert_eq!(table.tombstone_seq(ROOT), Some(5));
    }
}
