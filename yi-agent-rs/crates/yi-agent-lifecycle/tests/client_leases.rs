//! Frontend lease state machine.
//!
//! The table performs no I/O: time is a `Duration` from the server's monotonic
//! clock, supplied by the caller. That is what makes the awkward cases testable
//! without sleeping — a late EOF racing a resumed client, a heartbeat arriving
//! after release, a wall-clock jump that must *not* look like a user quit.

use std::time::Duration;

use yi_agent_lifecycle::leases::{DisconnectOutcome, LeaseTable, ReleaseOutcome};
use yi_agent_lifecycle::types::{ClientKind, LeaseMode};

/// Heartbeat interval and expiry window from the spec.
const HEARTBEAT: Duration = Duration::from_secs(5);
const EXPIRY: Duration = Duration::from_secs(30);

fn table() -> LeaseTable {
    LeaseTable::new()
}

fn register(
    table: &mut LeaseTable,
    id: &str,
    now: Duration,
) -> yi_agent_lifecycle::types::ClientGrant {
    table
        .register(id.into(), ClientKind::App, LeaseMode::Business, now)
        .unwrap()
}

// ---- registration ---------------------------------------------------------

#[test]
fn a_registered_client_is_counted_and_gets_a_credential() {
    let mut table = table();
    let grant = register(&mut table, "app", Duration::ZERO);
    assert_eq!(table.active_count(), 1);
    assert_eq!(grant.epoch, 1);
    // At least 256 bits, hex encoded: a guessable credential would let any local
    // process renew someone else's lease.
    assert!(grant.credential.len() >= 64, "{}", grant.credential);
    assert!(grant.credential.bytes().all(|b| b.is_ascii_hexdigit()));
}

#[test]
fn two_clients_get_different_credentials() {
    let mut table = table();
    let a = register(&mut table, "a", Duration::ZERO);
    let b = register(&mut table, "b", Duration::ZERO);
    assert_ne!(a.credential, b.credential);
    assert_ne!(a.client_run_id, b.client_run_id);
}

#[test]
fn re_registering_a_live_id_is_refused_and_must_resume_instead() {
    // A second `register` for a live id would let a new process steal a live
    // client's lease without proving it owns the credential.
    let mut table = table();
    let _first = register(&mut table, "app", Duration::ZERO);
    let error = table
        .register(
            "app".into(),
            ClientKind::App,
            LeaseMode::Business,
            Duration::from_secs(1),
        )
        .unwrap_err();
    assert!(error.contains("resume"), "{error}");
    assert_eq!(table.active_count(), 1);
}

#[test]
fn lease_only_and_waiting_clients_count_as_active() {
    // They keep the runtime alive; only read-only queries do not, and a query is
    // not a registration at all.
    let mut table = table();
    table
        .register(
            "obs".into(),
            ClientKind::App,
            LeaseMode::LeaseOnly,
            Duration::ZERO,
        )
        .unwrap();
    table
        .register(
            "waiting".into(),
            ClientKind::Tui,
            LeaseMode::WaitingNextGeneration,
            Duration::ZERO,
        )
        .unwrap();
    assert_eq!(table.active_count(), 2);
}

// ---- resume and late EOF --------------------------------------------------

#[test]
fn late_eof_does_not_remove_resumed_client() {
    let mut table = LeaseTable::new();
    let first = table
        .register(
            "app".into(),
            ClientKind::App,
            LeaseMode::Business,
            Duration::ZERO,
        )
        .unwrap();
    let resumed = table.resume(&first, Duration::from_secs(8)).unwrap();
    // The old connection's EOF arrives *after* the client already reattached on a
    // new connection. Treating it as "the client left" would reclaim a live lease.
    assert_eq!(
        table.disconnected("app", first.epoch),
        DisconnectOutcome::IgnoredStaleEpoch
    );
    table.heartbeat(&resumed, Duration::from_secs(29)).unwrap();
    assert!(table.expire(Duration::from_secs(31)).is_empty());
    assert_eq!(table.active_count(), 1);
}

#[test]
fn resume_bumps_the_epoch_and_rotates_the_credential() {
    let mut table = table();
    let first = register(&mut table, "app", Duration::ZERO);
    let resumed = table.resume(&first, Duration::from_secs(1)).unwrap();
    assert_eq!(resumed.epoch, first.epoch + 1);
    assert_ne!(resumed.credential, first.credential);
    // The old epoch's credential no longer authenticates anything.
    assert!(table.heartbeat(&first, Duration::from_secs(2)).is_err());
    // A second bump cannot reuse an epoch value either.
    let third = table.resume(&resumed, Duration::from_secs(3)).unwrap();
    assert_eq!(third.epoch, resumed.epoch + 1);
}

#[test]
fn a_current_eof_waits_for_reconnect_rather_than_reclaiming_at_once() {
    // The spec's rule: EOF does not renew the lease and does not reclaim
    // immediately. A dropped connection is often a reconnect, and killing the
    // user's work over a transient socket close is the failure this avoids.
    let mut table = table();
    let grant = register(&mut table, "app", Duration::ZERO);
    assert_eq!(
        table.disconnected("app", grant.epoch),
        DisconnectOutcome::AwaitReconnect
    );
    assert_eq!(table.active_count(), 1, "the lease survives the disconnect");
}

#[test]
fn eof_does_not_renew_the_lease() {
    let mut table = table();
    let grant = register(&mut table, "app", Duration::ZERO);
    table.disconnected("app", grant.epoch);
    // 30s after registration, with no heartbeat since, the lease expires — the EOF
    // neither extended it nor reclaimed it.
    assert_eq!(table.expire(EXPIRY), vec!["app".to_string()]);
}

#[test]
fn an_unregistered_id_eof_is_ignored() {
    let mut table = table();
    assert_eq!(
        table.disconnected("ghost", 1),
        DisconnectOutcome::IgnoredStaleEpoch
    );
}

// ---- heartbeat and expiry -------------------------------------------------

#[test]
fn heartbeat_extends_the_deadline() {
    let mut table = table();
    let grant = register(&mut table, "app", Duration::ZERO);
    table.heartbeat(&grant, Duration::from_secs(29)).unwrap();
    assert!(table.expire(Duration::from_secs(31)).is_empty());
    assert_eq!(
        table.expire(Duration::from_secs(60)),
        vec!["app".to_string()]
    );
}

#[test]
fn a_heartbeat_with_the_wrong_credential_is_refused_and_does_not_extend() {
    let mut table = table();
    let grant = register(&mut table, "app", Duration::ZERO);
    let mut forged = grant.clone();
    forged.credential = "0".repeat(64);
    assert!(table.heartbeat(&forged, EXPIRY).is_err());
    // Still expires: a rejected heartbeat must not be a free renewal.
    assert_eq!(table.expire(EXPIRY), vec!["app".to_string()]);
}

#[test]
fn a_late_heartbeat_after_release_cannot_revive_the_client() {
    // The reclaim has already been emitted. A straggling heartbeat must not bring
    // the lease back: the runtime would then be kept alive by a client whose work
    // was already reclaimed.
    let mut table = table();
    let grant = register(&mut table, "app", Duration::ZERO);
    assert_eq!(
        table.release(&grant).unwrap(),
        ReleaseOutcome::Reclaim("app".to_string())
    );
    assert!(table.heartbeat(&grant, Duration::from_secs(1)).is_err());
    assert_eq!(table.active_count(), 0);
    assert!(table.expire(Duration::from_secs(60)).is_empty());
}

#[test]
fn a_released_id_cannot_register_again() {
    // Its work was already reclaimed. Letting the same run id re-register would
    // resurrect a lease on behalf of work that is gone — the client must present a
    // genuinely new run id instead.
    let mut table = table();
    let grant = register(&mut table, "app", Duration::ZERO);
    table.release(&grant).unwrap();
    let error = table
        .register(
            "app".into(),
            ClientKind::App,
            LeaseMode::Business,
            Duration::from_secs(5),
        )
        .unwrap_err();
    assert!(error.contains("released"), "{error}");
    assert_eq!(table.active_count(), 0);

    // A different run id is fine.
    let fresh = register(&mut table, "app-2", Duration::from_secs(5));
    assert_eq!(fresh.epoch, 1);
    assert_ne!(fresh.credential, grant.credential);
}

#[test]
fn the_released_tombstone_table_stays_bounded() {
    // A misbehaving client that registers and releases in a loop must not grow the
    // table without limit. Tombstones are only needed long enough for a straggling
    // heartbeat or EOF to be recognised as stale.
    let mut table = table();
    for index in 0..(yi_agent_lifecycle::leases::MAX_TOMBSTONES * 2) {
        let id = format!("run-{index}");
        let grant = table
            .register(id, ClientKind::App, LeaseMode::Business, Duration::ZERO)
            .unwrap();
        table.release(&grant).unwrap();
    }
    assert_eq!(
        table.tombstone_count(),
        yi_agent_lifecycle::leases::MAX_TOMBSTONES
    );
}

#[test]
fn releasing_twice_reports_already_released() {
    let mut table = table();
    let grant = register(&mut table, "app", Duration::ZERO);
    assert_eq!(
        table.release(&grant).unwrap(),
        ReleaseOutcome::Reclaim("app".to_string())
    );
    assert_eq!(
        table.release(&grant).unwrap(),
        ReleaseOutcome::AlreadyReleased
    );
}

#[test]
fn release_with_a_stale_epoch_is_refused() {
    let mut table = table();
    let first = register(&mut table, "app", Duration::ZERO);
    let _resumed = table.resume(&first, Duration::from_secs(1)).unwrap();
    // The pre-resume grant still names a live client; honouring it would let the
    // old connection release the new one's lease.
    assert!(table.release(&first).is_err());
    assert_eq!(table.active_count(), 1);
}

#[test]
fn expire_returns_every_expired_client_and_only_those() {
    let mut table = table();
    let early = register(&mut table, "early", Duration::ZERO);
    let late = register(&mut table, "late", Duration::from_secs(20));
    drop(early);
    let _ = late;

    let mut expired = table.expire(Duration::from_secs(31));
    expired.sort();
    assert_eq!(expired, vec!["early".to_string()]);
    assert_eq!(table.active_count(), 1);
    // Expiry is terminal for that run: a heartbeat cannot revive it.
    assert_eq!(
        table.expire(Duration::from_secs(100)),
        vec!["late".to_string()]
    );
}

#[test]
fn expiry_is_idempotent() {
    let mut table = table();
    let _grant = register(&mut table, "app", Duration::ZERO);
    assert_eq!(table.expire(EXPIRY), vec!["app".to_string()]);
    assert!(table.expire(Duration::from_secs(120)).is_empty());
}

// ---- system resume --------------------------------------------------------

#[test]
fn a_system_resume_reopens_a_full_renewal_window() {
    // A laptop closed for an hour must not look like every client quitting. The
    // explicit resume event grants a fresh window; a wall-clock jump alone does
    // not, which is why this takes an explicit call.
    let mut table = table();
    let grant = register(&mut table, "app", Duration::ZERO);
    table.after_system_resume(Duration::from_secs(3600));

    // Still active, and now has a full window from the resume moment.
    assert_eq!(table.active_count(), 1);
    assert!(
        table
            .expire(Duration::from_secs(3600) + EXPIRY - Duration::from_secs(1))
            .is_empty()
    );
    table
        .heartbeat(&grant, Duration::from_secs(3600) + HEARTBEAT)
        .unwrap();
    assert_eq!(
        table.expire(Duration::from_secs(3600) + EXPIRY * 2),
        vec!["app".to_string()]
    );
}

#[test]
fn a_released_client_is_not_revived_by_a_system_resume() {
    let mut table = table();
    let grant = register(&mut table, "app", Duration::ZERO);
    table.release(&grant).unwrap();
    table.after_system_resume(Duration::from_secs(100));
    assert_eq!(table.active_count(), 0);
    assert!(table.expire(Duration::from_secs(200)).is_empty());
}

// ---- the grant type -------------------------------------------------------

#[test]
fn a_grant_debug_never_shows_the_credential() {
    let mut table = table();
    let grant = register(&mut table, "app", Duration::ZERO);
    let debug = format!("{grant:?}");
    assert!(!debug.contains(&grant.credential), "{debug}");
    assert!(debug.contains("<redacted>"), "{debug}");
}
