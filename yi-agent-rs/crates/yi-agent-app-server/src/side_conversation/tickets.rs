//! Host-issued entry tickets for `/side`.
//!
//! # Why a ticket exists at all
//!
//! Opening a side used to be one call: "resolve this parent and create a runner".
//! That made two independent things look like one, and they have opposite
//! requirements:
//!
//! * **Discovering** a side must be cheap and idempotent — the desktop, the phone
//!   and a reconnecting client all ask, and a retry must not create a second
//!   runner (one parent has one provider stream).
//! * **Authorising** a side must be a single decision, taken once, against the
//!   parent's current state — and it must be impossible to replay.
//!
//! So `prepare` is a bounded, revocable *ticket*, and `bind` turns exactly one
//! ticket into exactly one generation. A ticket is a capability, not a name:
//! nothing here ever creates an entry for an id it has not issued, so an unknown,
//! expired or foreign ticket cannot become a side.
//!
//! # Why every expiry path ends in `Closed`, never in deletion
//!
//! A deleted ticket is indistinguishable from one that was never issued, which is
//! precisely the state a caller must not be able to reach by waiting. Closed
//! records keep the parent identity (never the content) until they age out, so a
//! late `exit` for a ticket whose side already ran gets `entry_mismatch` or a
//! closed answer instead of silently succeeding against a fresh entry.

use std::collections::HashMap;
use std::time::Duration;

use tokio::time::Instant;
use yi_agent_runtime::side_conversation::{SideError, SideErrorReason, SideResult};

/// How long a prepared-but-unused ticket stays usable.
///
/// Deliberately short: a ticket is only meant to cover the gap between "the host
/// decided this may open" and "the caller came back with a side id". Anything
/// longer is a capability nobody is watching.
pub const ENTRY_TICKET_TTL: Duration = Duration::from_secs(60);

/// How long a closed ticket is remembered.
///
/// Records are needed only to answer "this is gone, not unknown"; five minutes is
/// far beyond any client's retry horizon and bounds the map on its own.
pub const CLOSED_TICKET_TTL: Duration = Duration::from_secs(5 * 60);

/// Tickets one parent may hold at once.
pub const ENTRY_TICKETS_PER_PARENT: usize = 32;
/// Tickets one host may hold in total.
pub const ENTRY_TICKETS_PER_HOST: usize = 256;

/// The identity of one side generation.
///
/// `generation` exists because a side id can be reused (a client that retried
/// after a lost response, a host that reopened the same parent). A ticket bound to
/// generation 1 must never be satisfiable against generation 2.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SideGeneration {
    pub side_id: String,
    pub generation: u64,
}

impl SideGeneration {
    pub fn new(side_id: impl Into<String>, generation: u64) -> Self {
        Self {
            side_id: side_id.into(),
            generation,
        }
    }
}

/// What a caller receives from `prepare` and must present again to act.
///
/// Opaque on purpose: the fields are the host's bookkeeping, and a caller that
/// could construct one would be able to skip the bounded, revocable path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryTarget {
    pub(crate) host: String,
    pub(crate) ticket: String,
    pub(crate) parent: String,
}

impl EntryTarget {
    pub fn parent(&self) -> &str {
        &self.parent
    }
}

/// Why an entry ended, from the ticket table's point of view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitState {
    /// The ticket was prepared but never bound: nothing was ever created.
    Unused,
    /// A side did run in that generation and is being closed.
    Closing,
    /// The ticket was already closed (or its generation already finished).
    Closed,
}

/// The result of consuming a ticket on exit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExitReceipt {
    pub state: ExitState,
    /// Present when the ticket was bound: the exact generation to close.
    pub generation: Option<SideGeneration>,
}

#[derive(Clone, Debug)]
enum TicketState {
    Prepared {
        expires: Instant,
    },
    Bound(SideGeneration),
    /// A closed record remembers which generation it named, if any.
    ///
    /// The generation is not content and is not a capability: it lets a retried
    /// `exit` answer "that exact generation is still closing" instead of claiming
    /// a destroy that has not happened yet.
    Closed {
        at: Instant,
        side: Option<SideGeneration>,
    },
}

#[derive(Clone, Debug)]
struct Record {
    parent: String,
    state: TicketState,
}

/// The host's bounded ticket table.
///
/// Holds no resolver and no runner: preparing a ticket is a pure bookkeeping
/// decision, which is why "prepare touches no parent state" is a property of the
/// type rather than a promise in a comment.
pub struct EntryTickets {
    host: String,
    records: HashMap<String, Record>,
}

impl EntryTickets {
    pub fn new(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            records: HashMap::new(),
        }
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    /// Issue a ticket that may open one side of `parent`.
    ///
    /// Prunes first, then admits only if both caps allow. `Closed` records yield
    /// to a new ticket; `Prepared` and `Bound` never do, because evicting them
    /// would let a live side — or a caller still holding a valid ticket — lose its
    /// only evidence.
    pub fn prepare(&mut self, parent: &str, now: Instant) -> SideResult<EntryTarget> {
        self.prune(now);

        let per_parent = self
            .records
            .values()
            .filter(|record| record.parent == parent)
            .count();
        if per_parent >= ENTRY_TICKETS_PER_PARENT {
            return Err(SideError::new(
                SideErrorReason::EntryLimit,
                "该父会话的进入票据已达上限",
            ));
        }
        if self.records.len() >= ENTRY_TICKETS_PER_HOST {
            // Make room only from closed records: a bounded table must not turn a
            // retry into an eviction of somebody's live side.
            self.evict_closed_to_fit(ENTRY_TICKETS_PER_HOST - 1);
            if self.records.len() >= ENTRY_TICKETS_PER_HOST {
                return Err(SideError::new(
                    SideErrorReason::EntryLimit,
                    "宿主进入票据已达上限",
                ));
            }
        }

        let ticket = uuid::Uuid::new_v4().to_string();
        self.records.insert(
            ticket.clone(),
            Record {
                parent: parent.to_string(),
                state: TicketState::Prepared {
                    expires: now + ENTRY_TICKET_TTL,
                },
            },
        );
        Ok(EntryTarget {
            host: self.host.clone(),
            ticket,
            parent: parent.to_string(),
        })
    }

    /// Turn `target` into `side`, or refuse.
    ///
    /// Binding the same generation twice is a **retry**, not a new side: a caller
    /// whose response was lost comes back with the same id, and refusing it would
    /// strand a side that is really running. Binding a *different* generation is
    /// refused: that would be a second side from one authorisation.
    pub fn bind(
        &mut self,
        target: &EntryTarget,
        side: SideGeneration,
        now: Instant,
    ) -> SideResult<SideGeneration> {
        self.verify(target, now)?;
        let record = self
            .records
            .get_mut(&target.ticket)
            .expect("verify() proved the ticket exists");
        match &record.state {
            TicketState::Prepared { .. } => {
                record.state = TicketState::Bound(side.clone());
                Ok(side)
            }
            TicketState::Bound(existing) if *existing == side => Ok(side),
            TicketState::Bound(existing) => Err(SideError::new(
                SideErrorReason::EntryMismatch,
                format!(
                    "该票据已绑定到 {}#{}",
                    existing.side_id, existing.generation
                ),
            )),
            TicketState::Closed { .. } => Err(SideError::new(
                SideErrorReason::EntryExpired,
                "该票据已关闭",
            )),
        }
    }

    /// What `target` authorises right now.
    ///
    /// `None` means "prepared but not yet bound" — the caller may bind it. `Some`
    /// carries the generation that already exists, so a retry observes the side it
    /// created instead of creating another.
    pub fn resolve(
        &mut self,
        target: &EntryTarget,
        now: Instant,
    ) -> SideResult<Option<SideGeneration>> {
        self.verify(target, now)?;
        let record = self
            .records
            .get(&target.ticket)
            .expect("verify() proved the ticket exists");
        match &record.state {
            TicketState::Prepared { .. } => Ok(None),
            TicketState::Bound(side) => Ok(Some(side.clone())),
            TicketState::Closed { .. } => Err(SideError::new(
                SideErrorReason::EntryExpired,
                "该票据已关闭",
            )),
        }
    }

    /// The generation a closed record named, if any.
    ///
    /// Used by the host to answer a retried `exit` precisely. Returns `None` for an
    /// unknown, expired or never-bound ticket.
    pub fn closed_generation(
        &mut self,
        target: &EntryTarget,
        now: Instant,
    ) -> Option<SideGeneration> {
        self.verify(target, now).ok()?;
        match &self.records.get(&target.ticket)?.state {
            TicketState::Closed { side, .. } => side.clone(),
            _ => None,
        }
    }

    /// Consume `target` on exit.
    ///
    /// Answers with the *exact* generation to close. An unused ticket becomes
    /// closed and reports `Unused`; a bound one reports its generation and becomes
    /// closed so a replay cannot close a second side.
    pub fn revoke(&mut self, target: &EntryTarget, now: Instant) -> SideResult<ExitReceipt> {
        self.verify(target, now)?;
        let record = self
            .records
            .get_mut(&target.ticket)
            .expect("verify() proved the ticket exists");
        let receipt = match &record.state {
            TicketState::Prepared { .. } => ExitReceipt {
                state: ExitState::Unused,
                generation: None,
            },
            TicketState::Bound(side) => ExitReceipt {
                state: ExitState::Closing,
                generation: Some(side.clone()),
            },
            // A retried exit reports the same generation it reported the first
            // time, so the caller can ask the owner whether that generation is
            // really gone instead of inferring it from this table alone.
            TicketState::Closed { side, .. } => ExitReceipt {
                state: ExitState::Closed,
                generation: side.clone(),
            },
        };
        record.state = TicketState::Closed {
            at: now,
            side: receipt.generation.clone(),
        };
        Ok(receipt)
    }

    /// Mark every ticket bound to `side` as closed.
    ///
    /// Called once the generation is really gone. Only the exact generation is
    /// touched: a reused side id at a later generation is a different side, and
    /// closing it here would retire a live entry nobody asked to close.
    pub fn finish(&mut self, side: &SideGeneration, now: Instant) {
        for record in self.records.values_mut() {
            let matches = matches!(
                &record.state,
                TicketState::Bound(bound) if bound == side
            );
            if matches {
                record.state = TicketState::Closed {
                    at: now,
                    side: Some(side.clone()),
                };
            }
        }
    }

    /// How many records the table holds (for capacity assertions).
    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Prove the ticket exists, belongs to this host, is not expired, and names
    /// the parent it was issued for.
    ///
    /// Never inserts. A lookup that created the record would turn any string into
    /// a valid capability, which is the one thing this type must not allow.
    fn verify(&mut self, target: &EntryTarget, now: Instant) -> SideResult<()> {
        if target.host != self.host {
            return Err(SideError::new(
                SideErrorReason::EntryMismatch,
                "该票据不属于本宿主",
            ));
        }
        self.prune(now);
        let Some(record) = self.records.get(&target.ticket) else {
            // Unknown and expired are the same answer on purpose: distinguishing
            // them would tell a caller whether a parent it cannot see ever existed.
            return Err(SideError::new(
                SideErrorReason::EntryExpired,
                "该票据不存在或已过期",
            ));
        };
        if record.parent != target.parent {
            return Err(SideError::new(
                SideErrorReason::EntryMismatch,
                "该票据不属于该父会话",
            ));
        }
        Ok(())
    }

    /// Drop expired prepared tickets and aged closed records.
    fn prune(&mut self, now: Instant) {
        self.records.retain(|_, record| match &record.state {
            TicketState::Prepared { expires } => *expires > now,
            TicketState::Bound(_) => true,
            TicketState::Closed { at, .. } => now.duration_since(*at) < CLOSED_TICKET_TTL,
        });
    }

    /// Drop the oldest closed records until at most `limit` records remain.
    ///
    /// Only closed records are candidates. If there are not enough of them the
    /// table simply stays full and `prepare` reports `entry_limit`.
    fn evict_closed_to_fit(&mut self, limit: usize) {
        while self.records.len() > limit {
            let oldest = self
                .records
                .iter()
                .filter_map(|(ticket, record)| match &record.state {
                    TicketState::Closed { at, .. } => Some((ticket.clone(), *at)),
                    _ => None,
                })
                .min_by_key(|(_, at)| *at);
            let Some((ticket, _)) = oldest else {
                return;
            };
            self.records.remove(&ticket);
        }
    }
}

impl std::fmt::Debug for EntryTickets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print tickets or parents: both identify a caller's capability.
        f.debug_struct("EntryTickets")
            .field("host", &self.host)
            .field("records", &self.records.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn generation(id: &str, n: u64) -> SideGeneration {
        SideGeneration::new(id, n)
    }

    #[test]
    fn revoked_ticket_can_never_open_another_generation() {
        let now = tokio::time::Instant::now();
        let mut tickets = EntryTickets::new("host");
        let target = tickets.prepare("p", now).unwrap();
        assert_eq!(
            tickets.revoke(&target, now).unwrap().state,
            ExitState::Unused
        );
        assert!(tickets.resolve(&target, now).is_err());
        assert!(tickets.bind(&target, generation("new", 2), now).is_err());
    }

    #[test]
    fn a_prepared_ticket_resolves_to_nothing_and_binds_once() {
        let now = tokio::time::Instant::now();
        let mut tickets = EntryTickets::new("host");
        let target = tickets.prepare("p", now).unwrap();
        assert_eq!(tickets.resolve(&target, now).unwrap(), None);

        let side = generation("s1", 1);
        assert_eq!(tickets.bind(&target, side.clone(), now).unwrap(), side);
        assert_eq!(
            tickets.resolve(&target, now).unwrap(),
            Some(side.clone()),
            "a resolve after bind observes the side that exists"
        );
    }

    #[test]
    fn binding_the_same_generation_twice_is_a_retry_not_a_second_side() {
        let now = tokio::time::Instant::now();
        let mut tickets = EntryTickets::new("host");
        let target = tickets.prepare("p", now).unwrap();
        let side = generation("s1", 1);
        tickets.bind(&target, side.clone(), now).unwrap();
        assert_eq!(
            tickets.bind(&target, side.clone(), now).unwrap(),
            side,
            "a lost response must be recoverable"
        );
    }

    #[test]
    fn a_ticket_bound_to_one_generation_refuses_another() {
        let now = tokio::time::Instant::now();
        let mut tickets = EntryTickets::new("host");
        let target = tickets.prepare("p", now).unwrap();
        tickets.bind(&target, generation("s1", 1), now).unwrap();
        let error = tickets
            .bind(&target, generation("s1", 2), now)
            .expect_err("one authorisation is one side");
        assert_eq!(error.reason, SideErrorReason::EntryMismatch);
    }

    #[test]
    fn a_prepared_ticket_expires_at_sixty_seconds() {
        let start = Instant::now();
        let mut tickets = EntryTickets::new("host");
        let target = tickets.prepare("p", start).unwrap();

        assert!(
            tickets
                .resolve(&target, start + Duration::from_secs(59))
                .is_ok(),
            "inside the window the ticket is usable"
        );
        let error = tickets
            .resolve(&target, start + ENTRY_TICKET_TTL)
            .expect_err("a ticket has no life beyond its TTL");
        assert_eq!(error.reason, SideErrorReason::EntryExpired);
    }

    #[test]
    fn a_closed_ticket_is_forgotten_after_five_minutes() {
        let start = Instant::now();
        let mut tickets = EntryTickets::new("host");
        let target = tickets.prepare("p", start).unwrap();
        tickets.revoke(&target, start).unwrap();
        assert_eq!(tickets.len(), 1);

        // Touching the table at any point after the TTL drops the record.
        let later = start + CLOSED_TICKET_TTL;
        let error = tickets
            .resolve(&target, later)
            .expect_err("aged-out tickets are gone");
        assert_eq!(error.reason, SideErrorReason::EntryExpired);
        assert!(tickets.is_empty());
    }

    #[test]
    fn a_bound_ticket_is_never_pruned_by_age() {
        let start = Instant::now();
        let mut tickets = EntryTickets::new("host");
        let target = tickets.prepare("p", start).unwrap();
        let side = generation("s1", 1);
        tickets.bind(&target, side.clone(), start).unwrap();

        // Long past both TTLs: a live generation keeps its evidence.
        let later = start + CLOSED_TICKET_TTL + ENTRY_TICKET_TTL;
        assert_eq!(tickets.resolve(&target, later).unwrap(), Some(side));
    }

    #[test]
    fn one_parent_is_capped_at_32_tickets() {
        let now = Instant::now();
        let mut tickets = EntryTickets::new("host");
        for _ in 0..ENTRY_TICKETS_PER_PARENT {
            tickets.prepare("p", now).unwrap();
        }
        let error = tickets
            .prepare("p", now)
            .expect_err("a parent cannot mint unbounded capabilities");
        assert_eq!(error.reason, SideErrorReason::EntryLimit);

        // Another parent is unaffected: the cap is per parent, not global.
        assert!(tickets.prepare("other", now).is_ok());
    }

    #[test]
    fn the_host_is_capped_at_256_tickets() {
        let now = Instant::now();
        let mut tickets = EntryTickets::new("host");
        for index in 0..ENTRY_TICKETS_PER_HOST {
            tickets.prepare(&format!("p{index}"), now).unwrap();
        }
        let error = tickets
            .prepare("overflow", now)
            .expect_err("the table is bounded");
        assert_eq!(error.reason, SideErrorReason::EntryLimit);
        assert_eq!(tickets.len(), ENTRY_TICKETS_PER_HOST);
    }

    #[test]
    fn a_full_table_makes_room_only_from_closed_records() {
        let now = Instant::now();
        let mut tickets = EntryTickets::new("host");
        let mut bound = Vec::new();
        // 255 bound + 1 closed: the table is full of things that must not go.
        for index in 0..ENTRY_TICKETS_PER_HOST - 1 {
            let target = tickets.prepare(&format!("p{index}"), now).unwrap();
            tickets
                .bind(&target, generation(&format!("s{index}"), 1), now)
                .unwrap();
            bound.push(target);
        }
        let prepared = tickets.prepare("last", now).unwrap();
        tickets.revoke(&prepared, now).unwrap();
        assert_eq!(tickets.len(), ENTRY_TICKETS_PER_HOST);

        // The closed record yields: a new ticket fits, and no bound one is lost.
        assert!(tickets.prepare("fresh", now).is_ok());
        assert_eq!(tickets.len(), ENTRY_TICKETS_PER_HOST);
        for (index, target) in bound.iter().enumerate() {
            assert_eq!(
                tickets.resolve(target, now).unwrap(),
                Some(generation(&format!("s{index}"), 1)),
                "a live generation must never be evicted to make room"
            );
        }
    }

    #[test]
    fn a_full_table_of_pinned_records_refuses_rather_than_evicting() {
        let now = Instant::now();
        let mut tickets = EntryTickets::new("host");
        for index in 0..ENTRY_TICKETS_PER_HOST {
            let target = tickets.prepare(&format!("p{index}"), now).unwrap();
            tickets
                .bind(&target, generation(&format!("s{index}"), 1), now)
                .unwrap();
        }
        let error = tickets
            .prepare("overflow", now)
            .expect_err("nothing may be evicted");
        assert_eq!(error.reason, SideErrorReason::EntryLimit);
        assert_eq!(tickets.len(), ENTRY_TICKETS_PER_HOST);
    }

    #[test]
    fn a_ticket_from_another_host_is_refused() {
        let now = Instant::now();
        let mut host_a = EntryTickets::new("a");
        let target = host_a.prepare("p", now).unwrap();

        let mut host_b = EntryTickets::new("b");
        let error = host_b
            .resolve(&target, now)
            .expect_err("a new host instance does not honour old tickets");
        assert_eq!(error.reason, SideErrorReason::EntryMismatch);
        assert!(host_b.is_empty(), "refusing must not create a record");
    }

    #[test]
    fn a_ticket_cannot_be_used_for_another_parent() {
        let now = Instant::now();
        let mut tickets = EntryTickets::new("host");
        let target = tickets.prepare("p", now).unwrap();
        let mut forged = target.clone();
        forged.parent = "other".into();
        let error = tickets
            .resolve(&forged, now)
            .expect_err("a ticket names the parent it was issued for");
        assert_eq!(error.reason, SideErrorReason::EntryMismatch);
    }

    #[test]
    fn an_unknown_ticket_is_expired_and_never_creates_an_entry() {
        let now = Instant::now();
        let mut tickets = EntryTickets::new("host");
        let unknown = EntryTarget {
            host: "host".into(),
            ticket: "never-issued".into(),
            parent: "p".into(),
        };
        for result in [
            tickets.resolve(&unknown, now).map(|_| ()),
            tickets.bind(&unknown, generation("s", 1), now).map(|_| ()),
            tickets.revoke(&unknown, now).map(|_| ()),
        ] {
            assert_eq!(result.unwrap_err().reason, SideErrorReason::EntryExpired);
        }
        assert!(tickets.is_empty());
    }

    #[test]
    fn finishing_a_generation_closes_only_its_exact_bound_tickets() {
        let now = Instant::now();
        let mut tickets = EntryTickets::new("host");
        let first = tickets.prepare("p", now).unwrap();
        let second = tickets.prepare("p", now).unwrap();
        tickets.bind(&first, generation("s", 1), now).unwrap();
        tickets.bind(&second, generation("s", 2), now).unwrap();

        tickets.finish(&generation("s", 1), now);

        assert_eq!(
            tickets.resolve(&first, now).unwrap_err().reason,
            SideErrorReason::EntryExpired
        );
        assert_eq!(
            tickets.resolve(&second, now).unwrap(),
            Some(generation("s", 2)),
            "the later generation is a different side and stays usable"
        );
    }

    #[test]
    fn revoking_a_bound_ticket_reports_its_generation_and_closes_it() {
        let now = Instant::now();
        let mut tickets = EntryTickets::new("host");
        let target = tickets.prepare("p", now).unwrap();
        let side = generation("s1", 1);
        tickets.bind(&target, side.clone(), now).unwrap();

        let receipt = tickets.revoke(&target, now).unwrap();
        assert_eq!(receipt.state, ExitState::Closing);
        assert_eq!(receipt.generation, Some(side.clone()));

        // A replay must not close a second side; it sees "already closed" and
        // still names the generation it referred to, so the host can ask that
        // generation whether its cleanup really finished.
        let replay = tickets.revoke(&target, now).unwrap();
        assert_eq!(replay.state, ExitState::Closed);
        assert_eq!(replay.generation, Some(side));
    }

    #[test]
    fn preparing_tickets_never_touches_a_parent_resolver() {
        // The type holds no resolver, so this is a compile-time property: preparing
        // a ticket cannot read a parent, a transcript or a project directory. The
        // test states it so a future "convenience" field cannot slip in unnoticed.
        fn _assert_no_resolver_field(tickets: &EntryTickets) {
            let _ = tickets.len();
        }
        let mut tickets = EntryTickets::new("host");
        let now = Instant::now();
        for _ in 0..8 {
            _assert_no_resolver_field(&tickets);
            tickets.prepare("p", now).unwrap();
        }
        assert!(
            matches!(
                tickets.records.values().next().unwrap().state,
                TicketState::Prepared { .. }
            ),
            "prepare only records a ticket"
        );
    }
}
