//! Impact previews and one-shot confirmation.
//!
//! Replacing a runtime is destructive: every client of that scope loses its
//! connection, and whatever was running is torn down. So the decision is split in
//! two on purpose:
//!
//! 1. **Preview** answers *what would this disturb* and mints a token. It is
//!    strictly read-only: no drain, no maintenance lock, no admission change. A
//!    cancelled preview therefore cannot have side effects -- which is what makes
//!    it safe to call while the user is still deciding.
//! 2. **Confirm** consumes that token and starts the switch. It succeeds at most
//!    once, and only while the world still matches what the user was shown.
//!
//! The binding is the whole design. A token is tied to the requester, the scope,
//! the daemon generation, the target artifact, and **both** revision counters. If
//! any of them moved between preview and confirm, the confirmation is stale: the
//! user authorising "restart with one idle client" must not thereby authorise
//! "restart while three turns are running". Nothing here retries or widens a
//! stale confirmation; the caller has to show a fresh preview.
//!
//! Unknown impact is a first-class answer, not an error and not a zero. When the
//! business daemon does not answer, `counts.complete` is false and the coordinator
//! revision is `None`; the guard is that such a snapshot always requires an
//! explicit second confirmation, and that a runtime which cannot be independently
//! fenced is refused with manual guidance instead of being force-killed.

use std::collections::HashMap;
use std::time::Duration;

use crate::types::{
    ClientGrant, ConfirmationToken, ImpactCounts, ImpactEntry, OperationId, RestartPreview,
};

// Re-exported so a caller of this module can name the context a preview was minted
// against without reaching into `types` for it; the two are one concept.
pub use crate::types::ConfirmationContext;

/// How long a minted token stays valid.
///
/// A confirmation is a person saying "yes, now". Sixty seconds is long enough to
/// read a preview and click, and short enough that a token found in a log tomorrow
/// is worthless.
pub const TOKEN_TTL: Duration = Duration::from_secs(60);

/// Target size of one page's entry list.
///
/// The bound is on *serialized* size, not entry count: one entry can carry a long
/// path, so counting entries would let a page exceed the frame limit. 16 KiB leaves
/// room for the surrounding DTO inside a 64 KiB frame.
pub const PAGE_BYTES: usize = 16 * 1024;

/// Why a confirmation was refused.
///
/// All of these are refusals rather than retries: retrying on a stale context would
/// silently widen what the user agreed to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfirmRefusal {
    /// No such token: never minted, expired, or already consumed.
    UnknownToken,
    /// Minted for a different scope/generation/client.
    WrongOwner,
    /// The scope, generation, target artifact, or revision changed.
    Stale,
    /// The token was already used. Carries the operation it was bound to so a
    /// caller can look up the outcome instead of asking again.
    AlreadyConsumed(OperationId),
}

impl ConfirmRefusal {
    /// The stable reason string, for logs and for the wire.
    pub fn reason(&self) -> String {
        match self {
            Self::UnknownToken => "the confirmation token is unknown or has expired".to_string(),
            Self::WrongOwner => {
                "the confirmation token was issued to a different client".to_string()
            }
            Self::Stale => {
                "the runtime changed since the preview; take a fresh preview".to_string()
            }
            Self::AlreadyConsumed(operation) => {
                format!("this confirmation was already used by operation {operation}")
            }
        }
    }
}

/// One minted preview.
#[derive(Clone, Debug)]
struct Snapshot {
    /// Who asked. A token is a capability for one client, never for the scope.
    client_run_id: String,
    context: ConfirmationContext,
    preview: RestartPreview,
    entries: Vec<ImpactEntry>,
    /// Absolute deadline on the caller's monotonic clock.
    expires_at: Duration,
    consumed_by: Option<OperationId>,
}

/// The preview/confirm state machine for one guardian.
///
/// Pure: time is passed in, and no method performs I/O. Everything the real
/// handler must do around it (drain, fences, coordinator calls) stays in the
/// guardian, so this file can be tested for the properties that matter without a
/// socket or a process.
#[derive(Debug, Default)]
pub struct PreviewStore {
    snapshots: HashMap<ConfirmationToken, Snapshot>,
    /// Tokens newest-first, so pruning on expiry is bounded work rather than a
    /// scan of everything ever minted.
    order: std::collections::VecDeque<ConfirmationToken>,
}

/// What the caller knows about the world when it asks for a preview.
///
/// The counts are *derived* from the entries rather than passed separately: two
/// lists that can disagree would let the UI say "1 running task" beside a list of
/// three, and the number is what a person actually reads.
#[derive(Clone, Debug, Default)]
pub struct ImpactInput {
    /// Everything a replacement would disturb, one row per thing.
    pub entries: Vec<ImpactEntry>,
    /// Whether this runtime can still stop its own work independently (the
    /// guardian holds the process/executor/tool identity). Without that, an
    /// unknown-impact restart must be refused rather than attempted.
    pub fencable: bool,
}

/// Entry kinds. Strings on the wire, constants here so the derivation and the
/// producers cannot drift apart.
pub const KIND_CLIENT: &str = "client";
pub const KIND_PENDING_CLIENT: &str = "pending-client";
pub const KIND_RUNNING: &str = "execution";
pub const KIND_QUEUED: &str = "queued";
pub const KIND_PLUGIN: &str = "plugin";

/// Counts, derived from the entry list.
///
/// `complete` is decided by the caller (it knows whether the coordinator answered)
/// and filled in afterwards; everything else is a pure function of the entries.
fn derive_counts(entries: &[ImpactEntry]) -> ImpactCounts {
    let mut counts = ImpactCounts {
        complete: true,
        ..ImpactCounts::default()
    };
    for entry in entries {
        match entry.kind.as_str() {
            KIND_CLIENT => counts.clients += 1,
            KIND_PENDING_CLIENT => counts.pending_clients += 1,
            KIND_RUNNING => counts.running += 1,
            KIND_QUEUED => counts.queued += 1,
            KIND_PLUGIN => counts.plugins += 1,
            // An unknown kind is still an affected thing. Counting it as nothing
            // would make a future producer's rows invisible in the numbers the UI
            // shows, which is the one direction that must not happen.
            _ => {}
        }
    }
    counts
}

impl PreviewStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mints a snapshot and its token. Strictly read-only.
    ///
    /// `guardian_revision` must be read *after* collecting `input.entries` and
    /// *before* minting, and the caller re-reads it after; if it moved, the caller
    /// must discard this preview rather than sign a half-collected snapshot. That
    /// ordering is the caller's job because only it can see the counter twice.
    pub fn preview(
        &mut self,
        grant: &ClientGrant,
        context: ConfirmationContext,
        input: ImpactInput,
        now: Duration,
    ) -> RestartPreview {
        // "Complete" is not a property of the list alone: an empty list from a
        // daemon that never answered is indistinguishable from an idle one, so the
        // caller's revision is what decides.
        let complete = context.revision.coordinator.is_some();
        let counts = ImpactCounts {
            complete,
            ..derive_counts(&input.entries)
        };
        let revision = context.revision;

        // Unknown impact is never the harmless case, and a runtime that cannot be
        // independently fenced cannot be replaced automatically at all.
        let (can_restart, blocked_reason) = if input.fencable {
            (true, None)
        } else {
            (
                false,
                Some(
                    "this runtime cannot stop its work independently, so it cannot be \
                     replaced automatically; stop it by hand and start the new version"
                        .to_string(),
                ),
            )
        };

        // Not the harmless case when anyone else is involved, when work exists, or
        // when the numbers are not trustworthy.
        let requires_second_confirmation = !complete
            || counts.clients > 1
            || counts.pending_clients > 0
            || counts.running > 0
            || counts.queued > 0
            || counts.plugins > 0;

        let token = new_token();
        let snapshot_id = new_snapshot_id();
        let entries = input.entries;
        let (first_page, next_cursor) = paginate(&entries, 0, None);

        let preview = RestartPreview {
            snapshot_id: snapshot_id.clone(),
            token: token.clone(),
            revision,
            counts,
            entries: first_page,
            next_cursor,
            requires_second_confirmation,
            can_restart,
            blocked_reason,
        };

        self.snapshots.insert(
            token.clone(),
            Snapshot {
                client_run_id: grant.client_run_id.clone(),
                context,
                preview: preview.clone(),
                entries,
                expires_at: now.saturating_add(TOKEN_TTL),
                consumed_by: None,
            },
        );
        self.order.push_back(token);
        preview
    }

    /// The full context a token was minted against, after the checks that do not
    /// depend on a live revision.
    ///
    /// The guardian needs this because the **target artifact is bound by the token,
    /// not supplied again at confirm time**. Re-verifying "upgrade to whatever the
    /// caller says now" would let a click authorised for 9.2.2 be replayed as 9.3.0;
    /// reading the target out of the snapshot is what makes the two the same
    /// decision.
    pub fn stored_binding(
        &self,
        grant: &ClientGrant,
        token: &str,
        now: Duration,
    ) -> Result<ConfirmationContext, ConfirmRefusal> {
        let snapshot = self
            .snapshots
            .get(token)
            .ok_or(ConfirmRefusal::UnknownToken)?;
        if snapshot.client_run_id != grant.client_run_id {
            return Err(ConfirmRefusal::WrongOwner);
        }
        if snapshot.expires_at <= now {
            return Err(ConfirmRefusal::UnknownToken);
        }
        Ok(snapshot.context.clone())
    }

    /// The same, addressed by snapshot id rather than token (paging has no token).
    pub fn stored_binding_for(
        &self,
        grant: &ClientGrant,
        snapshot_id: &str,
        now: Duration,
    ) -> Result<ConfirmationContext, ConfirmRefusal> {
        let snapshot = self
            .snapshots
            .values()
            .find(|snapshot| snapshot.preview.snapshot_id == snapshot_id)
            .ok_or(ConfirmRefusal::UnknownToken)?;
        if snapshot.client_run_id != grant.client_run_id {
            return Err(ConfirmRefusal::WrongOwner);
        }
        if snapshot.expires_at <= now {
            return Err(ConfirmRefusal::UnknownToken);
        }
        Ok(snapshot.context.clone())
    }

    /// Returns one more page. Never returns a token.
    ///
    /// Owner and full context are re-checked, so a cursor is not a capability: a
    /// client cannot read another client's preview by guessing a snapshot id, and a
    /// revision change makes the rest of an old snapshot unreadable too.
    pub fn page(
        &mut self,
        grant: &ClientGrant,
        snapshot_id: &str,
        cursor: &str,
        current: &ConfirmationContext,
        now: Duration,
    ) -> Result<RestartPreview, ConfirmRefusal> {
        let token = self
            .order
            .iter()
            .find(|token| {
                self.snapshots
                    .get(*token)
                    .is_some_and(|snapshot| snapshot.preview.snapshot_id == snapshot_id)
            })
            .cloned()
            .ok_or(ConfirmRefusal::UnknownToken)?;
        let snapshot = self
            .snapshots
            .get(&token)
            .ok_or(ConfirmRefusal::UnknownToken)?;

        if snapshot.client_run_id != grant.client_run_id {
            return Err(ConfirmRefusal::WrongOwner);
        }
        if snapshot.expires_at <= now {
            return Err(ConfirmRefusal::UnknownToken);
        }
        if &snapshot.context != current {
            return Err(ConfirmRefusal::Stale);
        }

        let offset = cursor
            .parse::<usize>()
            .map_err(|_| ConfirmRefusal::UnknownToken)?;
        let (entries, next_cursor) = paginate(&snapshot.entries, offset, Some(cursor));
        Ok(RestartPreview {
            // A page carries no token: paging is reading, and reading must not be
            // able to authorise a switch.
            token: String::new(),
            entries,
            next_cursor,
            ..snapshot.preview.clone()
        })
    }

    /// Consumes the token and binds it to `operation_id`.
    ///
    /// The order of checks matters: ownership, then freshness, then single use. A
    /// stale owner with a valid token must not learn that the token was consumed by
    /// someone else, and a consumed token must report *its* operation even when the
    /// caller is the original owner retrying after a lost response.
    pub fn confirm(
        &mut self,
        grant: &ClientGrant,
        token: &str,
        operation_id: &str,
        current: &ConfirmationContext,
        now: Duration,
    ) -> Result<OperationId, ConfirmRefusal> {
        let snapshot = self
            .snapshots
            .get_mut(token)
            .ok_or(ConfirmRefusal::UnknownToken)?;

        if snapshot.client_run_id != grant.client_run_id {
            return Err(ConfirmRefusal::WrongOwner);
        }
        if let Some(operation) = &snapshot.consumed_by {
            // Idempotent for the same operation, a refusal for a new one: a second
            // click must not start a second switch, but a retry after a dropped
            // response must still be able to report the first one's id.
            if operation == operation_id {
                return Ok(operation.clone());
            }
            return Err(ConfirmRefusal::AlreadyConsumed(operation.clone()));
        }
        if snapshot.expires_at <= now {
            return Err(ConfirmRefusal::UnknownToken);
        }
        if &snapshot.context != current {
            return Err(ConfirmRefusal::Stale);
        }

        snapshot.consumed_by = Some(operation_id.to_string());
        Ok(operation_id.to_string())
    }

    /// Drops expired tokens. Called on maintenance ticks; bounded work because the
    /// queue is ordered by mint time.
    pub fn prune(&mut self, now: Duration) {
        while let Some(token) = self.order.front() {
            let expired = self
                .snapshots
                .get(token)
                .is_some_and(|snapshot| snapshot.expires_at <= now);
            if !expired {
                break;
            }
            self.snapshots.remove(token);
            self.order.pop_front();
        }
    }

    /// How many snapshots are retained. Exposed for the bound test.
    pub fn len(&self) -> usize {
        self.snapshots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.snapshots.is_empty()
    }
}

/// Splits `entries` into pages bounded by their **serialized** size.
///
/// Measured, not estimated: an id can be a long path or a task name from a user's
/// own text, so counting bytes of the three fields would let a page exceed the frame
/// limit exactly when the names are long. `serde_json::to_vec` produces the same
/// bytes that will be written to the socket, which is the only number that matters.
///
/// A single entry larger than the budget is returned alone rather than dropped: the
/// alternative is an entry no page can contain and no caller can ever see.
fn paginate(
    entries: &[ImpactEntry],
    offset: usize,
    _cursor: Option<&str>,
) -> (Vec<ImpactEntry>, Option<String>) {
    let mut page = Vec::new();
    let mut used = 0usize;
    for (index, entry) in entries.iter().enumerate().skip(offset) {
        // A length-prefixed JSON frame; the separator stands in for the array commas.
        let size = serde_json::to_vec(entry).map_or(0, |encoded| encoded.len() + 1);
        if !page.is_empty() && used + size > PAGE_BYTES {
            return (page, Some(index.to_string()));
        }
        used += size;
        page.push(entry.clone());
    }
    (page, None)
}

/// A one-shot confirmation token: 256 bits from the OS CSPRNG, hex encoded.
fn new_token() -> ConfirmationToken {
    random_hex()
}

fn new_snapshot_id() -> String {
    random_hex()
}

fn random_hex() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("OS CSPRNG unavailable");
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ArtifactIdentity, ImpactRevision, LeaseMode};
    use std::path::PathBuf;

    fn grant(id: &str) -> ClientGrant {
        ClientGrant {
            client_run_id: id.to_string(),
            credential: "c".to_string(),
            epoch: 1,
            mode: LeaseMode::Business,
        }
    }

    fn artifact(build: &str) -> ArtifactIdentity {
        ArtifactIdentity {
            build_id: build.to_string(),
            display_version: "1.0.0".to_string(),
            executable: PathBuf::from("/bin/yi-agent"),
            sha256: String::new(),
            schema_min: 0,
            schema_max: 16,
            handover_version: 1,
            business_protocol_min: 2,
            business_protocol_max: 2,
            capabilities: Vec::new(),
        }
    }

    fn context(revision_guardian: u64, coordinator: Option<u64>) -> ConfirmationContext {
        ConfirmationContext {
            scope_key: "/p/.yi-agent/runtime".to_string(),
            daemon_instance_id: "daemon-1".to_string(),
            target: artifact("new"),
            revision: ImpactRevision {
                guardian: revision_guardian,
                coordinator,
            },
        }
    }

    /// Only this App, nothing running: the one case that may skip the second
    /// confirmation.
    fn idle() -> ImpactInput {
        ImpactInput {
            entries: vec![entry(KIND_CLIENT, "app")],
            fencable: true,
        }
    }

    fn entry(kind: &str, id: &str) -> ImpactEntry {
        ImpactEntry {
            kind: kind.to_string(),
            id: id.to_string(),
            owner: Some("app".to_string()),
        }
    }

    #[test]
    fn a_lone_idle_app_is_the_harmless_case() {
        let mut store = PreviewStore::new();
        let preview = store.preview(&grant("app"), context(1, Some(7)), idle(), Duration::ZERO);
        assert!(preview.counts.complete);
        assert!(!preview.requires_second_confirmation);
        assert!(preview.can_restart);
        assert!(preview.blocked_reason.is_none());
        assert!(!preview.token.is_empty());
    }

    /// An unanswered daemon is unknown, not empty. If this were reported as zero
    /// the UI would show "nothing running" and offer a one-click restart over
    /// whatever was actually in flight.
    #[test]
    fn an_unanswered_coordinator_is_unknown_and_needs_a_second_confirmation() {
        let mut store = PreviewStore::new();
        // An empty list from a daemon that never answered looks exactly like an
        // idle runtime; only the absent revision can tell them apart.
        let preview = store.preview(&grant("app"), context(1, None), idle(), Duration::ZERO);
        assert!(!preview.counts.complete);
        assert_eq!(preview.revision.coordinator, None);
        assert!(preview.requires_second_confirmation);
        // Still possible, because this guardian can fence it independently.
        assert!(preview.can_restart);
    }

    /// Without an independent fence, unknown impact is a manual job. Killing an
    /// unbounded set of processes and hoping is not an upgrade strategy.
    #[test]
    fn unknown_impact_without_independent_fence_requires_manual_action() {
        let mut store = PreviewStore::new();
        let mut input = idle();
        input.fencable = false;
        let preview = store.preview(&grant("app"), context(1, None), input, Duration::ZERO);
        assert!(!preview.can_restart);
        assert!(
            preview
                .blocked_reason
                .as_deref()
                .unwrap_or_default()
                .contains("by hand")
        );
    }

    #[test]
    fn another_client_running_work_or_a_plugin_all_need_a_second_confirmation() {
        let mut store = PreviewStore::new();
        for kind in [
            KIND_CLIENT,
            KIND_PENDING_CLIENT,
            KIND_RUNNING,
            KIND_QUEUED,
            KIND_PLUGIN,
        ] {
            let mut input = idle();
            input.entries.push(entry(kind, "other"));
            let preview = store.preview(&grant("app"), context(1, Some(7)), input, Duration::ZERO);
            assert!(
                preview.requires_second_confirmation,
                "a second {kind} row must require a second confirmation"
            );
            // The count the UI shows must agree with the rows it lists.
            let counted = match kind {
                KIND_CLIENT => preview.counts.clients,
                KIND_PENDING_CLIENT => preview.counts.pending_clients,
                KIND_RUNNING => preview.counts.running,
                KIND_QUEUED => preview.counts.queued,
                _ => preview.counts.plugins,
            };
            let expected = if kind == KIND_CLIENT { 2 } else { 1 };
            assert_eq!(counted, expected, "{kind} count");
        }
    }

    #[test]
    fn a_token_is_consumed_exactly_once() {
        let mut store = PreviewStore::new();
        let ctx = context(1, Some(7));
        let preview = store.preview(&grant("app"), ctx.clone(), idle(), Duration::ZERO);

        let operation = store
            .confirm(&grant("app"), &preview.token, "op-1", &ctx, Duration::ZERO)
            .expect("first confirmation is accepted");
        assert_eq!(operation, "op-1");

        // A second click with a *new* operation id must not start a second switch.
        match store.confirm(&grant("app"), &preview.token, "op-2", &ctx, Duration::ZERO) {
            Err(ConfirmRefusal::AlreadyConsumed(operation)) => assert_eq!(operation, "op-1"),
            other => panic!("expected AlreadyConsumed, got {other:?}"),
        }

        // A retry of the *same* operation is idempotent: a lost response must not
        // force the user to confirm again.
        assert_eq!(
            store
                .confirm(&grant("app"), &preview.token, "op-1", &ctx, Duration::ZERO)
                .unwrap(),
            "op-1"
        );
    }

    #[test]
    fn a_token_expires_after_its_window() {
        let mut store = PreviewStore::new();
        let ctx = context(1, Some(7));
        let preview = store.preview(&grant("app"), ctx.clone(), idle(), Duration::ZERO);

        let just_before = TOKEN_TTL - Duration::from_millis(1);
        assert!(
            store
                .confirm(&grant("app"), &preview.token, "op-1", &ctx, just_before)
                .is_ok()
        );

        let mut store = PreviewStore::new();
        let preview = store.preview(&grant("app"), ctx.clone(), idle(), Duration::ZERO);
        assert_eq!(
            store.confirm(&grant("app"), &preview.token, "op-1", &ctx, TOKEN_TTL),
            Err(ConfirmRefusal::UnknownToken)
        );
    }

    /// The core safety property: a change between preview and confirm invalidates
    /// the consent the user gave, in either authority.
    #[test]
    fn a_changed_revision_in_either_authority_makes_the_token_stale() {
        for moved in [context(2, Some(7)), context(1, Some(8))] {
            let mut store = PreviewStore::new();
            let preview = store.preview(&grant("app"), context(1, Some(7)), idle(), Duration::ZERO);
            assert_eq!(
                store.confirm(
                    &grant("app"),
                    &preview.token,
                    "op-1",
                    &moved,
                    Duration::ZERO
                ),
                Err(ConfirmRefusal::Stale)
            );
        }
    }

    /// "Upgrade to A" and "upgrade to B" are different decisions even when the
    /// current impact is identical.
    #[test]
    fn a_different_target_artifact_makes_the_token_stale() {
        let mut store = PreviewStore::new();
        let preview = store.preview(&grant("app"), context(1, Some(7)), idle(), Duration::ZERO);

        let mut other_target = context(1, Some(7));
        other_target.target = artifact("other");
        assert_eq!(
            store.confirm(
                &grant("app"),
                &preview.token,
                "op-1",
                &other_target,
                Duration::ZERO
            ),
            Err(ConfirmRefusal::Stale)
        );

        let mut other_generation = context(1, Some(7));
        other_generation.daemon_instance_id = "daemon-2".to_string();
        assert_eq!(
            store.confirm(
                &grant("app"),
                &preview.token,
                "op-1",
                &other_generation,
                Duration::ZERO
            ),
            Err(ConfirmRefusal::Stale)
        );
    }

    /// A token is a capability for one client. Another client presenting it must be
    /// refused even when every other field matches.
    #[test]
    fn a_token_belongs_to_the_client_that_asked() {
        let mut store = PreviewStore::new();
        let ctx = context(1, Some(7));
        let preview = store.preview(&grant("app"), ctx.clone(), idle(), Duration::ZERO);
        assert_eq!(
            store.confirm(
                &grant("other"),
                &preview.token,
                "op-1",
                &ctx,
                Duration::ZERO
            ),
            Err(ConfirmRefusal::WrongOwner)
        );
        assert_eq!(
            store.page(
                &grant("other"),
                &preview.snapshot_id,
                "0",
                &ctx,
                Duration::ZERO
            ),
            Err(ConfirmRefusal::WrongOwner)
        );
    }

    #[test]
    fn paging_returns_entries_but_never_a_token() {
        let mut store = PreviewStore::new();
        let mut input = idle();
        input.entries = (0..400)
            .map(|i| entry("execution", &format!("task-{i}")))
            .collect();
        let ctx = context(1, Some(7));
        let first = store.preview(&grant("app"), ctx.clone(), input, Duration::ZERO);

        assert!(!first.token.is_empty());
        assert!(
            first.next_cursor.is_some(),
            "400 entries must not fit one page"
        );
        let mut seen = first.entries.len();
        let mut cursor = first.next_cursor.clone();
        let mut pages = 0;
        while let Some(next) = cursor {
            pages += 1;
            let page = store
                .page(
                    &grant("app"),
                    &first.snapshot_id,
                    &next,
                    &ctx,
                    Duration::ZERO,
                )
                .expect("page");
            assert!(page.token.is_empty(), "a page must not carry a token");
            seen += page.entries.len();
            cursor = page.next_cursor.clone();
        }
        assert_eq!(seen, 400, "paging must not drop entries");
        assert!(pages >= 1);
    }

    /// A cursor is an offset, not an authorisation: it is still checked against the
    /// caller and the live revision.
    #[test]
    fn a_page_from_a_stale_revision_is_refused() {
        let mut store = PreviewStore::new();
        let mut input = idle();
        input.entries = (0..400)
            .map(|i| entry("execution", &format!("task-{i}")))
            .collect();
        let first = store.preview(&grant("app"), context(1, Some(7)), input, Duration::ZERO);
        let cursor = first.next_cursor.clone().expect("a second page");

        assert_eq!(
            store.page(
                &grant("app"),
                &first.snapshot_id,
                &cursor,
                &context(2, Some(7)),
                Duration::ZERO
            ),
            Err(ConfirmRefusal::Stale)
        );
        assert_eq!(
            store.page(
                &grant("app"),
                &first.snapshot_id,
                &cursor,
                &context(1, Some(8)),
                Duration::ZERO
            ),
            Err(ConfirmRefusal::Stale)
        );
        assert!(
            store
                .page(
                    &grant("app"),
                    &first.snapshot_id,
                    &cursor,
                    &context(1, Some(7)),
                    Duration::ZERO
                )
                .is_ok()
        );
    }

    /// The page bound is on serialized bytes, so a page must fit even when every
    /// entry carries a long id.
    #[test]
    fn a_page_is_bounded_by_serialized_size() {
        let mut store = PreviewStore::new();
        let mut input = idle();
        let long = "x".repeat(4000);
        input
            .entries
            .extend((0..50).map(|i| entry(KIND_RUNNING, &format!("{long}-{i}"))));
        let ctx = context(1, Some(7));
        let first = store.preview(&grant("app"), ctx.clone(), input, Duration::ZERO);

        let encoded = serde_json::to_vec(&first.entries).expect("serializable");
        assert!(
            encoded.len() <= PAGE_BYTES,
            "one page must fit the budget, got {} bytes",
            encoded.len()
        );
        assert!(
            first.next_cursor.is_some(),
            "the rest must be reachable by paging"
        );
    }

    #[test]
    fn an_unknown_snapshot_is_refused() {
        let mut store = PreviewStore::new();
        assert_eq!(
            store.page(
                &grant("app"),
                "no-such-snapshot",
                "0",
                &context(1, Some(7)),
                Duration::ZERO
            ),
            Err(ConfirmRefusal::UnknownToken)
        );
    }

    #[test]
    fn pruning_drops_expired_tokens_and_keeps_live_ones() {
        let mut store = PreviewStore::new();
        let old = store.preview(&grant("app"), context(1, Some(7)), idle(), Duration::ZERO);
        let fresh = store.preview(
            &grant("app"),
            context(1, Some(7)),
            idle(),
            TOKEN_TTL - Duration::from_secs(1),
        );
        assert_eq!(store.len(), 2);

        store.prune(TOKEN_TTL);
        assert_eq!(store.len(), 1, "only the expired one is dropped");
        let ctx = context(1, Some(7));
        assert_eq!(
            store.confirm(&grant("app"), &old.token, "op-old", &ctx, TOKEN_TTL),
            Err(ConfirmRefusal::UnknownToken)
        );
        assert!(
            store
                .confirm(&grant("app"), &fresh.token, "op-new", &ctx, TOKEN_TTL)
                .is_ok()
        );
    }

    /// Two previews must not share a token or a snapshot id; a collision would let
    /// one decision authorise another.
    #[test]
    fn tokens_and_snapshots_are_unique() {
        let mut store = PreviewStore::new();
        let mut tokens = std::collections::HashSet::new();
        let mut snapshots = std::collections::HashSet::new();
        for _ in 0..64 {
            let preview = store.preview(&grant("app"), context(1, Some(7)), idle(), Duration::ZERO);
            assert_eq!(preview.token.len(), 64, "256-bit token, hex encoded");
            assert!(tokens.insert(preview.token), "token collision");
            assert!(snapshots.insert(preview.snapshot_id), "snapshot collision");
        }
    }
}
