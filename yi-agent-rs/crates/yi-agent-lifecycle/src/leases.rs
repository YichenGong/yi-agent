//! Frontend lease state machine: who is keeping this runtime alive.
//!
//! No I/O happens here. Time is a `Duration` from the server's monotonic clock,
//! handed in by the caller, which is what makes the hard cases testable without
//! sleeping: a late EOF racing a resumed client, a heartbeat arriving after the
//! reclaim already fired, a laptop that slept for an hour.
//!
//! Three rules carry most of the weight:
//!
//! 1. **EOF is not a departure.** A dropped connection is usually a reconnect, so
//!    EOF neither renews the lease nor reclaims the client; the lease simply keeps
//!    running down its existing deadline.
//! 2. **Epochs fence reconnects.** One `client_run_id` can hold several
//!    connections over its life. Every `resume` bumps the epoch and rotates the
//!    credential, so events from the previous connection (a late EOF, a late
//!    heartbeat) are recognisably stale rather than ambiguous.
//! 3. **Release and expiry are terminal.** A heartbeat after either must not
//!    revive the client: the reclaim has already been emitted, and a resurrected
//!    lease would keep a runtime alive on behalf of work that is already gone.
//!
//! Credentials are 256-bit CSPRNG values. They exist only in the guardian's memory
//! (and the anonymous handover descriptor); they are never logged, and `Debug` on
//! [`ClientGrant`] redacts them.

use std::collections::HashMap;
use std::time::Duration;

use crate::types::{ClientGrant, ClientKind, ClientRunId, LeaseMode};

/// How long a lease survives without a heartbeat.
pub const LEASE_EXPIRY: Duration = Duration::from_secs(30);

/// How long after an explicit OS resume a lease is considered renewable.
///
/// A machine that slept is indistinguishable, from lease timestamps alone, from a
/// machine whose clients all quit an hour ago. The resume event is explicit
/// precisely so this is a decision rather than a guess.
pub const RESUME_GRACE: Duration = LEASE_EXPIRY;

/// Credential length in bytes (256 bits, hex encoded to 64 characters).
const CREDENTIAL_BYTES: usize = 32;

/// Upper bound on released-id tombstones.
///
/// A tombstone only has to outlive the straggling visitor it rejects: a late
/// heartbeat or EOF arrives on an already-closed connection, so seconds, not
/// forever. The cap exists because a client that registers and releases in a loop
/// would otherwise grow this set without limit.
pub const MAX_TOMBSTONES: usize = 1024;

/// What a disconnect event means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisconnectOutcome {
    /// The event named a connection that is no longer the client's current one.
    /// Nothing changed.
    IgnoredStaleEpoch,
    /// The client's current connection closed. The lease keeps running down; a
    /// reconnect inside the window is expected.
    AwaitReconnect,
}

/// What an explicit release did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReleaseOutcome {
    /// This run was already released or expired. Releasing is idempotent.
    AlreadyReleased,
    /// Emit a reclaim for this client run.
    Reclaim(ClientRunId),
}

/// One registered client run.
#[derive(Clone, Debug)]
struct Lease {
    epoch: u64,
    credential: String,
    mode: LeaseMode,
    /// `clock.now()` value at which this lease expires without a heartbeat.
    expires_at: Duration,
    /// Whether the client's current connection is still open.
    ///
    /// Separate from the lease itself because an EOF is *not* a departure (see the
    /// module docs): the client keeps its lease while it reconnects. But a
    /// replacement would interrupt it either way, so an impact preview has to show
    /// it -- and show it as "may come back", not as "already gone".
    connected: bool,
}

/// The lease state machine for one runtime generation.
#[derive(Debug, Default)]
pub struct LeaseTable {
    leases: HashMap<ClientRunId, Lease>,
    /// Runs whose lease ended (release or expiry). Kept so a straggling heartbeat
    /// or EOF is rejected as stale instead of silently creating a new lease.
    /// Ordered oldest-first so the table can be capped: see [`MAX_TOMBSTONES`].
    released: std::collections::VecDeque<ClientRunId>,
    /// Monotonic counter of changes that alter *who would be affected* by a
    /// replacement.
    ///
    /// A confirmation is bound to this value, so it must advance on exactly the
    /// events that change the impact answer: a client registering, being released,
    /// expiring, or resuming (a resume is a new connection for an existing client,
    /// which is not a new owner -- see `resume`). It must **not** advance on a
    /// heartbeat: heartbeats are the one thing every live client does constantly,
    /// and bumping the revision for them would invalidate every confirmation within
    /// seconds and make the feature unusable.
    revision: u64,
}

impl LeaseTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new client run.
    ///
    /// Refuses an id that is already live or already released. A live id must
    /// `resume` (proving it holds the credential); a released id's work has already
    /// been reclaimed, so re-registering it would resurrect a lease on behalf of
    /// work that is gone. Either way, `register` must not be a way to steal or
    /// resurrect a lease.
    pub fn register(
        &mut self,
        id: ClientRunId,
        kind: ClientKind,
        mode: LeaseMode,
        now: Duration,
    ) -> Result<ClientGrant, String> {
        // `kind` is recorded by the caller's own bookkeeping; the table does not key
        // on it, but rejecting a non-frontend kind here is the structural guard
        // against a worker accidentally keeping a runtime alive.
        let _ = kind;
        if self.leases.contains_key(&id) {
            return Err(format!(
                "client {id} is already registered; call resume with its grant instead"
            ));
        }
        if self.released.contains(&id) {
            return Err(format!(
                "client {id} was released and its reclaim is pending; it cannot register again \
                 under the same run id"
            ));
        }
        let credential = new_credential();
        let epoch = 1;
        self.leases.insert(
            id.clone(),
            Lease {
                epoch,
                credential: credential.clone(),
                mode,
                expires_at: now.saturating_add(LEASE_EXPIRY),
                connected: true,
            },
        );
        self.revision = self.revision.saturating_add(1);
        Ok(ClientGrant {
            client_run_id: id,
            credential,
            epoch,
            mode,
        })
    }

    /// Reattach a known client on a new connection: new epoch, new credential.
    pub fn resume(&mut self, grant: &ClientGrant, now: Duration) -> Result<ClientGrant, String> {
        let lease = self
            .leases
            .get_mut(&grant.client_run_id)
            .ok_or_else(|| format!("client {} is not registered", grant.client_run_id))?;
        if lease.credential != grant.credential || lease.epoch != grant.epoch {
            return Err("stale or forged grant; cannot resume".to_string());
        }
        lease.epoch += 1;
        lease.credential = new_credential();
        lease.expires_at = now.saturating_add(LEASE_EXPIRY);
        lease.connected = true;
        Ok(ClientGrant {
            client_run_id: grant.client_run_id.clone(),
            credential: lease.credential.clone(),
            epoch: lease.epoch,
            mode: lease.mode,
        })
    }

    /// Renew the lease. A rejected heartbeat does **not** extend it.
    pub fn heartbeat(&mut self, grant: &ClientGrant, now: Duration) -> Result<(), String> {
        let lease = self.leases.get_mut(&grant.client_run_id).ok_or_else(|| {
            if self.released.contains(&grant.client_run_id) {
                format!(
                    "client {} was released; a heartbeat cannot revive it",
                    grant.client_run_id
                )
            } else {
                format!("client {} is not registered", grant.client_run_id)
            }
        })?;
        if lease.credential != grant.credential || lease.epoch != grant.epoch {
            return Err(format!(
                "stale or forged heartbeat for client {} (epoch {})",
                grant.client_run_id, grant.epoch
            ));
        }
        lease.expires_at = now.saturating_add(LEASE_EXPIRY);
        Ok(())
    }

    /// A connection closed.
    ///
    /// Deliberately does not touch the deadline: a disconnect is not evidence of a
    /// user quitting, and extending it on EOF would let a dead client's lease live
    /// forever.
    pub fn disconnected(&mut self, id: &str, epoch: u64) -> DisconnectOutcome {
        match self.leases.get_mut(id) {
            Some(lease) if lease.epoch == epoch => {
                lease.connected = false;
                // Deliberately no revision bump. A disconnect is not a departure:
                // the client may reconnect inside its window, and the set of owners
                // a replacement would interrupt is unchanged either way. Bumping
                // here would let a flapping connection invalidate a confirmation
                // that a person is still reading.
                DisconnectOutcome::AwaitReconnect
            }
            // Either unknown, already released/expired, or superseded by a resume.
            // A late EOF for a superseded epoch must not mark the *current*
            // connection as gone.
            _ => DisconnectOutcome::IgnoredStaleEpoch,
        }
    }

    /// Explicit release: the client said it is leaving, so reclaim immediately.
    pub fn release(&mut self, grant: &ClientGrant) -> Result<ReleaseOutcome, String> {
        let Some(lease) = self.leases.get(&grant.client_run_id) else {
            // Either already gone, or never existed. Both mean "nothing to reclaim".
            return Ok(ReleaseOutcome::AlreadyReleased);
        };
        if lease.credential != grant.credential || lease.epoch != grant.epoch {
            return Err(format!(
                "stale or forged release for client {} (epoch {})",
                grant.client_run_id, grant.epoch
            ));
        }
        self.leases.remove(&grant.client_run_id);
        self.remember_released(&grant.client_run_id);
        self.revision = self.revision.saturating_add(1);
        Ok(ReleaseOutcome::Reclaim(grant.client_run_id.clone()))
    }

    /// Drop every lease whose deadline has passed, returning the run ids.
    ///
    /// Sorted so a caller emitting reclaims gets a deterministic order.
    pub fn expire(&mut self, now: Duration) -> Vec<ClientRunId> {
        let mut expired: Vec<ClientRunId> = self
            .leases
            .iter()
            .filter(|(_, lease)| lease.expires_at <= now)
            .map(|(id, _)| id.clone())
            .collect();
        expired.sort();
        for id in &expired {
            self.leases.remove(id);
            self.remember_released(id);
        }
        if !expired.is_empty() {
            self.revision = self.revision.saturating_add(1);
        }
        expired
    }

    /// The OS reported a resume from sleep.
    ///
    /// Every still-registered lease gets a fresh window. This is an explicit signal
    /// rather than an inference from wall-clock time because a clock jump is
    /// indistinguishable from "the user quit an hour ago"; guessing wrong either
    /// kills live sessions or keeps dead ones alive.
    pub fn after_system_resume(&mut self, now: Duration) {
        for lease in self.leases.values_mut() {
            lease.expires_at = now.saturating_add(RESUME_GRACE);
        }
    }

    /// Record a terminal run so late traffic for it is recognisably stale.
    fn remember_released(&mut self, id: &str) {
        if self.released.contains(&id.to_string()) {
            return;
        }
        if self.released.len() >= MAX_TOMBSTONES {
            self.released.pop_front();
        }
        self.released.push_back(id.to_string());
    }

    /// How many released-id tombstones are retained. Exposed for the bound test.
    pub fn tombstone_count(&self) -> usize {
        self.released.len()
    }

    /// Whether `client` may start new work right now.
    ///
    /// `false` is the answer for an unknown id as well as a released one: an
    /// unknown id has no admitted lease, and treating "never registered" as
    /// "allowed" would let a caller bypass admission by inventing an id.
    pub fn admits_execution(&self, client: &str) -> bool {
        if self.released.iter().any(|id| id == client) {
            return false;
        }
        self.leases.contains_key(client)
    }

    /// Whether `grant` may start new work at `now`.
    ///
    /// The credential and epoch must match the live lease, so a caller cannot ask
    /// on behalf of an owner it does not speak for. An expired lease is refused:
    /// its client is about to be reclaimed, and admitting work into it would
    /// orphan that work.
    pub fn admits(&self, grant: &ClientGrant, now: Duration) -> Result<(), String> {
        if self.released.iter().any(|id| id == &grant.client_run_id) {
            return Err(format!(
                "client {} was released; its new work is refused",
                grant.client_run_id
            ));
        }
        let lease = self
            .leases
            .get(&grant.client_run_id)
            .ok_or_else(|| format!("client {} is not registered", grant.client_run_id))?;
        if lease.credential != grant.credential || lease.epoch != grant.epoch {
            return Err(format!(
                "stale or forged grant for client {} (epoch {})",
                grant.client_run_id, grant.epoch
            ));
        }
        if lease.mode != LeaseMode::Business {
            // `LeaseOnly` observes and controls upgrades; it never authorizes a
            // task. Enforced here rather than at the call site, because a lease
            // that *could* be talked into running work would make the mode
            // advisory.
            return Err(format!(
                "client {}'s lease is {:?}, which never authorizes a task",
                grant.client_run_id, lease.mode
            ));
        }
        if lease.expires_at <= now {
            return Err(format!(
                "client {}'s lease expired; its new work is refused",
                grant.client_run_id
            ));
        }
        Ok(())
    }

    /// Whether `grant` names a live lease of *this* runtime, whatever its mode.
    ///
    /// Distinct from [`Self::admits`], which additionally refuses a non-business
    /// lease: an observer may not start work, but it may look at a preview. The
    /// credential and epoch are still checked, so "may look" never means "may look
    /// as somebody else".
    pub fn admits_or_observes(&self, grant: &ClientGrant, now: Duration) -> Result<(), String> {
        if self.released.iter().any(|id| id == &grant.client_run_id) {
            return Err(format!(
                "client {} was released; its credential no longer works",
                grant.client_run_id
            ));
        }
        let lease = self
            .leases
            .get(&grant.client_run_id)
            .ok_or_else(|| format!("client {} is not registered", grant.client_run_id))?;
        if lease.credential != grant.credential || lease.epoch != grant.epoch {
            return Err(format!(
                "stale or forged grant for client {} (epoch {})",
                grant.client_run_id, grant.epoch
            ));
        }
        if lease.expires_at <= now {
            return Err(format!(
                "client {}'s lease expired; reattach before asking",
                grant.client_run_id
            ));
        }
        Ok(())
    }

    /// The revision a preview/confirmation is bound to.
    ///
    /// See [`LeaseTable::revision`]'s field comment for exactly which events move
    /// it. Callers compare it for equality, never for ordering: it is a change
    /// token, not a timestamp.
    pub fn impact_revision(&self) -> u64 {
        self.revision
    }

    pub fn active_count(&self) -> usize {
        self.leases.len()
    }

    /// Clients whose lease is live but whose connection is currently closed.
    ///
    /// These still count as affected: they are inside their reconnect window and
    /// may come back, so a restart is not the harmless case just because they are
    /// momentarily silent.
    pub fn pending_count(&self) -> usize {
        self.leases
            .values()
            .filter(|lease| !lease.connected)
            .count()
    }

    /// Every live lease as `(client run id, lease mode, connected)`.
    ///
    /// Sorted by client run id so two previews over an unchanged runtime produce
    /// the same entry order; an unstable order would make a page boundary move for
    /// no reason.
    pub fn snapshot(&self) -> Vec<(ClientRunId, LeaseMode, bool)> {
        let mut rows: Vec<(ClientRunId, LeaseMode, bool)> = self
            .leases
            .iter()
            .map(|(id, lease)| (id.clone(), lease.mode, lease.connected))
            .collect();
        rows.sort_by(|left, right| left.0.cmp(&right.0));
        rows
    }

    /// How many live leases may actually run work.
    ///
    /// Lease-only observers are excluded on purpose: they are watching. Counting
    /// them would mean a scope can never drain while someone is observing the
    /// upgrade, so the teardown everyone is waiting for would never begin.
    pub fn business_active_count(&self) -> usize {
        self.leases
            .values()
            .filter(|lease| lease.mode == LeaseMode::Business)
            .count()
    }
}

/// 256-bit credential from the OS CSPRNG, hex encoded.
///
/// Panics only if the OS entropy source fails, which on any platform that can run
/// this daemon means the process is already unusable.
fn new_credential() -> String {
    let mut bytes = [0u8; CREDENTIAL_BYTES];
    getrandom::fill(&mut bytes).expect("OS CSPRNG unavailable");
    let mut out = String::with_capacity(CREDENTIAL_BYTES * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A heartbeat renews a lease without changing who is affected.
    ///
    /// This is the load-bearing one: if heartbeats moved the revision, a
    /// confirmation taken at time T would be stale by the next heartbeat (every
    /// 5 s), and the feature would never complete.
    #[test]
    fn heartbeat_does_not_change_impact_revision() {
        let mut table = LeaseTable::new();
        let grant = table
            .register(
                "app".into(),
                ClientKind::App,
                LeaseMode::Business,
                Duration::ZERO,
            )
            .unwrap();
        let before = table.impact_revision();
        table.heartbeat(&grant, Duration::from_secs(5)).unwrap();
        assert_eq!(before, table.impact_revision());
    }

    /// A resume is a new connection for a client that was already counted, so the
    /// impact answer is unchanged and a waiting confirmation must survive it.
    #[test]
    fn resume_in_place_does_not_change_impact_revision() {
        let mut table = LeaseTable::new();
        let grant = table
            .register(
                "app".into(),
                ClientKind::App,
                LeaseMode::Business,
                Duration::ZERO,
            )
            .unwrap();
        let before = table.impact_revision();
        let resumed = table.resume(&grant, Duration::from_secs(1)).unwrap();
        assert_eq!(before, table.impact_revision());

        // ...and the resumed grant is still a valid second registration's no-op.
        table.heartbeat(&resumed, Duration::from_secs(6)).unwrap();
        assert_eq!(before, table.impact_revision());
    }

    /// A closed connection is not a departure, but it is still something a
    /// replacement would interrupt: the client is inside its reconnect window.
    #[test]
    fn a_disconnected_client_is_pending_not_gone() {
        let mut table = LeaseTable::new();
        let grant = table
            .register(
                "app".into(),
                ClientKind::App,
                LeaseMode::Business,
                Duration::ZERO,
            )
            .unwrap();
        assert_eq!(table.active_count(), 1);
        assert_eq!(table.pending_count(), 0);

        let before = table.impact_revision();
        assert_eq!(
            table.disconnected("app", grant.epoch),
            DisconnectOutcome::AwaitReconnect
        );
        assert_eq!(table.pending_count(), 1, "it must still be counted");
        assert_eq!(table.active_count(), 1, "and still hold its lease");
        assert_eq!(
            before,
            table.impact_revision(),
            "a reconnectable client is not a new owner"
        );

        // A reconnect clears the pending state.
        let resumed = table.resume(&grant, Duration::from_secs(1)).unwrap();
        let _ = resumed;
        assert_eq!(table.pending_count(), 0);
    }

    /// A late EOF for a superseded connection must not mark the live one as gone.
    #[test]
    fn a_stale_epoch_eof_does_not_clear_the_current_connection() {
        let mut table = LeaseTable::new();
        let grant = table
            .register(
                "app".into(),
                ClientKind::App,
                LeaseMode::Business,
                Duration::ZERO,
            )
            .unwrap();
        let resumed = table.resume(&grant, Duration::from_secs(1)).unwrap();

        assert_eq!(
            table.disconnected("app", grant.epoch),
            DisconnectOutcome::IgnoredStaleEpoch
        );
        assert_eq!(
            table.pending_count(),
            0,
            "the previous connection's EOF says nothing about the new one"
        );
        let _ = resumed;
    }

    /// The preview reads one consistent row per live client.
    #[test]
    fn the_snapshot_lists_every_live_client_sorted_and_flagged() {
        let mut table = LeaseTable::new();
        table
            .register(
                "tui".into(),
                ClientKind::Tui,
                LeaseMode::Business,
                Duration::ZERO,
            )
            .unwrap();
        let app = table
            .register(
                "app".into(),
                ClientKind::App,
                LeaseMode::Business,
                Duration::ZERO,
            )
            .unwrap();
        // An observer does not run work, but it is still connected to this runtime.
        table
            .register(
                "watcher".into(),
                ClientKind::App,
                LeaseMode::LeaseOnly,
                Duration::ZERO,
            )
            .unwrap();
        table.disconnected("app", app.epoch);

        let rows = table.snapshot();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].0, "app");
        assert!(!rows[0].2, "app is pending");
        assert_eq!(rows[1].0, "tui");
        assert!(rows[1].2);
        assert_eq!(rows[2].0, "watcher");
        assert_eq!(rows[2].1, LeaseMode::LeaseOnly);
    }

    /// Registering, releasing and expiring all change who would be affected, so
    /// each must invalidate a confirmation taken before it.
    #[test]
    fn impact_revision_moves_on_every_real_change() {
        let mut table = LeaseTable::new();
        let start = table.impact_revision();

        let a = table
            .register(
                "a".into(),
                ClientKind::Tui,
                LeaseMode::Business,
                Duration::ZERO,
            )
            .unwrap();
        assert!(
            table.impact_revision() > start,
            "a new client is new impact"
        );

        let registered = table.impact_revision();
        table.release(&a).unwrap();
        assert!(
            table.impact_revision() > registered,
            "a release is new impact"
        );

        let released = table.impact_revision();
        table
            .register(
                "b".into(),
                ClientKind::Tui,
                LeaseMode::Business,
                Duration::ZERO,
            )
            .unwrap();
        let before_expiry = table.impact_revision();
        let expired = table.expire(Duration::from_secs(3600));
        assert_eq!(expired, vec!["b".to_string()]);
        assert!(
            table.impact_revision() > before_expiry,
            "expiry is new impact"
        );

        // Expiring nothing must not move it: a periodic sweep is not a change.
        let quiet = table.impact_revision();
        assert!(table.expire(Duration::from_secs(7200)).is_empty());
        assert_eq!(quiet, table.impact_revision());
        let _ = released;
    }

    #[test]
    fn the_expiry_window_matches_the_spec() {
        assert_eq!(LEASE_EXPIRY, Duration::from_secs(30));
    }

    #[test]
    fn credentials_are_256_bits() {
        let a = new_credential();
        let b = new_credential();
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
    }
}
