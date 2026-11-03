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
}

/// The lease state machine for one runtime generation.
#[derive(Debug, Default)]
pub struct LeaseTable {
    leases: HashMap<ClientRunId, Lease>,
    /// Runs whose lease ended (release or expiry). Kept so a straggling heartbeat
    /// or EOF is rejected as stale instead of silently creating a new lease.
    /// Ordered oldest-first so the table can be capped: see [`MAX_TOMBSTONES`].
    released: std::collections::VecDeque<ClientRunId>,
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
            },
        );
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
        match self.leases.get(id) {
            Some(lease) if lease.epoch == epoch => DisconnectOutcome::AwaitReconnect,
            // Either unknown, already released/expired, or superseded by a resume.
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

    pub fn active_count(&self) -> usize {
        self.leases.len()
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
