//! Building an impact snapshot from what the guardian can actually prove.
//!
//! The guardian owns leases and process groups. It does **not** own the business
//! daemon's executor state, plugin demand, or task queue, and it cannot see them
//! from another process. That asymmetry drives the whole shape of this module:
//!
//! - Rows the guardian can verify itself (clients, managed process groups) are
//!   always included.
//! - Rows only the coordinator knows are included **only** when the caller supplies
//!   a coordinator answer, together with its revision. No answer means no rows and
//!   `coordinator: None`, which the preview store renders as "unknown", never as
//!   "nothing running".
//!
//! Two rules keep a snapshot from signing a half-read world:
//!
//! 1. Both revisions are checked around the collection. The guardian revision is
//!    read before collecting and again after; the coordinator's revision is read
//!    with its rows and again after. If either moved, the snapshot is discarded
//!    rather than signed. A confirmation must describe one instant, and the daemon
//!    can change its executor state just as easily as the guardian can change its
//!    leases.
//! 2. A managed process group whose owner is not a live lease still appears, marked
//!    with whatever owner the registry recorded. Hiding it because its owner is
//!    gone would make a leftover group from a crashed client invisible in exactly
//!    the preview that is about to decide whether it is safe to stop everything.

use yi_agent_lifecycle::leases::LeaseTable;
use yi_agent_lifecycle::preview::{
    ImpactInput, KIND_CLIENT, KIND_PENDING_CLIENT, KIND_PLUGIN, KIND_RUNNING,
};
use yi_agent_lifecycle::types::{ArtifactIdentity, ImpactEntry, LeaseMode, RuntimeScope};

use super::demands::PluginDemandTable;
use super::guardian::{ProcessOwner, ProcessRecord, RecordState};

/// What only the business daemon can answer.
///
/// `revision` is the daemon's own counter over the same events the guardian counts
/// (clients joining/leaving, executions starting/stopping, plugin demand changing).
/// It is `None` when the daemon did not answer, and that is a fact the preview must
/// carry, not an empty answer to be smoothed over.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CoordinatorImpact {
    pub revision: Option<u64>,
    /// Executions running right now.
    pub running: Vec<ImpactEntry>,
    /// Executions queued but not started.
    pub queued: Vec<ImpactEntry>,
    /// Plugin services with at least one owner, from the daemon's demand table.
    pub plugins: Vec<ImpactEntry>,
}

impl CoordinatorImpact {
    /// The daemon did not answer within the budget.
    pub fn unanswered() -> Self {
        Self::default()
    }

    /// Whether this describes a known state.
    pub fn is_known(&self) -> bool {
        self.revision.is_some()
    }
}

/// Asks the business daemon what only it knows.
///
/// A trait rather than a direct socket call because the guardian must be able to run
/// without a daemon: `LeaseOnly` observers, tests, and the window during a version
/// switch all have a guardian and no healthy business endpoint. A probe that cannot
/// answer returns [`CoordinatorImpact::unanswered`], and the preview says "unknown"
/// instead of pretending the runtime is idle.
pub trait CoordinatorProbe: Send + Sync {
    /// One read, bounded by the caller's budget. Must not block on a wedged daemon:
    /// an unanswered probe is a valid, expected answer.
    fn impact(&self) -> CoordinatorImpact;
}

/// A probe for a runtime with no business endpoint to ask.
///
/// Every preview from this probe reports unknown impact, which is exactly what the
/// spec requires when the daemon cannot be reached: never "nothing is running".
pub struct NoCoordinator;

impl CoordinatorProbe for NoCoordinator {
    fn impact(&self) -> CoordinatorImpact {
        CoordinatorImpact::unanswered()
    }
}

/// Everything needed to render one preview, gathered from one consistent instant.
#[derive(Clone, Debug)]
pub struct CollectedImpact {
    pub input: ImpactInput,
    /// The guardian's own revision at the instant the snapshot was taken.
    pub guardian_revision: u64,
    /// Present only when the coordinator answered.
    pub coordinator_revision: Option<u64>,
}

/// Collects a snapshot, refusing to sign one that raced a change.
///
/// `read_revision` is called twice around the collection. Passing it in rather than
/// taking a `&Guardian` keeps this testable without a socket, and makes the
/// read-pair ordering explicit at the call site instead of hidden inside.
pub fn collect(
    leases: &LeaseTable,
    registry: &[ProcessRecord],
    demands: &PluginDemandTable,
    coordinator: CoordinatorImpact,
    fencable: bool,
    read_revision: impl Fn() -> u64,
    recheck_coordinator: impl Fn() -> Option<u64>,
) -> Result<CollectedImpact, SnapshotRace> {
    let before = read_revision();
    let mut entries = Vec::new();

    // 1. Clients the guardian can verify. A `LeaseOnly` observer never runs work,
    //    but it is connected to this runtime and a restart disconnects it, so it is
    //    listed as an affected client. It is not listed as *work*.
    for (client, mode, connected) in leases.snapshot() {
        let kind = if connected {
            KIND_CLIENT
        } else {
            // A closed connection is not a departure: the lease is still live, so
            // the client may reconnect into the runtime a restart is about to
            // replace. Showing it as "already gone" would understate the impact.
            KIND_PENDING_CLIENT
        };
        entries.push(ImpactEntry {
            kind: kind.to_string(),
            id: client,
            owner: Some(
                match mode {
                    LeaseMode::Business => "business",
                    LeaseMode::LeaseOnly => "lease-only",
                    LeaseMode::WaitingNextGeneration => "waiting-next-generation",
                }
                .to_string(),
            ),
        });
    }

    // 2. Managed process groups the guardian owns. These are the one thing the
    //    daemon cannot know and the guardian can prove, so they are added even when
    //    the coordinator is silent: they are how an unknown-impact preview still
    //    shows a real lower bound instead of nothing.
    for record in registry {
        if record.state != RecordState::Running {
            continue;
        }
        entries.push(ImpactEntry {
            kind: KIND_RUNNING.to_string(),
            id: record.process_id.clone(),
            owner: Some(owner_label(&record.owner)),
        });
    }

    // 3. Plugin demand the guardian's own table knows about. A plugin with demand
    //    is a service a restart stops.
    for plugin in demands.wanted_plugins() {
        let owners = demands.owners(&plugin);
        entries.push(ImpactEntry {
            kind: KIND_PLUGIN.to_string(),
            id: plugin,
            owner: owners.first().cloned(),
        });
    }

    // 4. What only the daemon knows, and only if it answered.
    if coordinator.is_known() {
        entries.extend(coordinator.running.iter().cloned());
        entries.extend(coordinator.queued.iter().cloned());
        entries.extend(coordinator.plugins.iter().cloned());
    }

    // A stable order so an unchanged runtime pages identically.
    entries.sort_by(|left, right| {
        (left.kind.as_str(), left.id.as_str())
            .cmp(&(right.kind.as_str(), right.id.as_str()))
            .then_with(|| left.owner.cmp(&right.owner))
    });

    let after = read_revision();
    if before != after {
        // Something changed while we were reading. Signing this would bind a
        // confirmation to a mixture of two instants, which is precisely what the
        // revision check exists to prevent.
        return Err(SnapshotRace { before, after });
    }
    // The daemon's rows were read before this collection started; the guardian's
    // revision cannot see a task starting or finishing, so re-reading the daemon's
    // revision is the only way to prove its rows still describe one instant. An
    // answer that turns from known to unknown is also a race: the rows cannot be
    // trusted and must not be signed as "what is running".
    let coordinator_now = recheck_coordinator();
    if coordinator_now != coordinator.revision {
        return Err(SnapshotRace {
            before: coordinator.revision.unwrap_or(before),
            after: coordinator_now.unwrap_or(after),
        });
    }

    Ok(CollectedImpact {
        input: ImpactInput { entries, fencable },
        guardian_revision: after,
        coordinator_revision: coordinator.revision,
    })
}

/// The revision moved while a snapshot was being collected.
///
/// Not an error to retry blindly: the caller re-collects, and the new snapshot may
/// show a different impact that the user has not seen yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotRace {
    pub before: u64,
    pub after: u64,
}

impl std::fmt::Display for SnapshotRace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the runtime changed while its impact was being measured \
             (revision {} -> {}); take a fresh preview",
            self.before, self.after
        )
    }
}

/// A human-readable owner for a process record: a client id, a plugin name, or the
/// daemon generation. Never a credential.
fn owner_label(owner: &ProcessOwner) -> String {
    match owner {
        ProcessOwner::Frontend(id) => id.clone(),
        ProcessOwner::SharedPlugin(name) => format!("plugin:{name}"),
        ProcessOwner::Daemon(id) => format!("daemon:{id}"),
    }
}

/// Whether this guardian can independently fence everything it manages.
///
/// True only for a managed runtime whose registry was opened (so the guardian holds
/// the child handles and PGIDs) *and* whose target artifact declares a handover
/// version this build understands. Without both, an unknown-impact restart must be
/// refused with manual guidance rather than attempted: the alternative is killing
/// by name and hoping.
pub fn is_fencable(registry_attached: bool, target: &ArtifactIdentity) -> bool {
    registry_attached && target.handover_version >= 1
}

/// The identity half of a `ConfirmationContext`, before a revision is known.
pub fn context_prefix(
    scope: &RuntimeScope,
    daemon_instance_id: impl Into<String>,
    target: ArtifactIdentity,
) -> (String, String, ArtifactIdentity) {
    (scope.key.clone(), daemon_instance_id.into(), target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::time::Duration;
    use yi_agent_lifecycle::preview::KIND_QUEUED;
    use yi_agent_lifecycle::types::ClientKind;

    fn leases_with(clients: &[(&str, LeaseMode)]) -> LeaseTable {
        let mut table = LeaseTable::new();
        for (id, mode) in clients {
            table
                .register((*id).into(), ClientKind::App, *mode, Duration::ZERO)
                .unwrap();
        }
        table
    }

    fn record(id: &str, owner: ProcessOwner, state: RecordState) -> ProcessRecord {
        ProcessRecord {
            scope_key: "/p".to_string(),
            generation: "g1".to_string(),
            process_id: id.to_string(),
            pid: 1,
            pgid: 1,
            start_micros: 0,
            artifact_build_id: "b".to_string(),
            artifact_sha256: String::new(),
            owner,
            root: None,
            execution: None,
            state,
            fault: None,
        }
    }

    fn collect_with(
        leases: &LeaseTable,
        records: &[ProcessRecord],
        demands: &PluginDemandTable,
        coordinator: CoordinatorImpact,
        revision: u64,
    ) -> Result<CollectedImpact, SnapshotRace> {
        let expected = coordinator.revision;
        collect(
            leases,
            records,
            demands,
            coordinator,
            true,
            || revision,
            || expected,
        )
    }

    #[test]
    fn clients_and_pending_clients_are_both_listed() {
        let mut leases = leases_with(&[("app", LeaseMode::Business), ("tui", LeaseMode::Business)]);
        leases.disconnected("tui", 1);

        let collected = collect_with(
            &leases,
            &[],
            &PluginDemandTable::new(),
            CoordinatorImpact::unanswered(),
            1,
        )
        .unwrap();

        let kinds: Vec<&str> = collected
            .input
            .entries
            .iter()
            .map(|entry| entry.kind.as_str())
            .collect();
        assert!(kinds.contains(&KIND_CLIENT));
        assert!(
            kinds.contains(&KIND_PENDING_CLIENT),
            "a disconnected client inside its window is still affected"
        );
    }

    /// The guardian's own evidence must survive a silent daemon; otherwise an
    /// unknown-impact preview would claim nothing is happening while managed
    /// groups are still running.
    #[test]
    fn managed_groups_appear_even_when_the_daemon_never_answered() {
        let leases = leases_with(&[("app", LeaseMode::Business)]);
        let records = vec![
            record("p1", ProcessOwner::frontend("app"), RecordState::Running),
            record("p2", ProcessOwner::plugin("github"), RecordState::Running),
            // A stopped record is history, not impact.
            record("p3", ProcessOwner::frontend("app"), RecordState::Reclaimed),
        ];
        let collected = collect_with(
            &leases,
            &records,
            &PluginDemandTable::new(),
            CoordinatorImpact::unanswered(),
            1,
        )
        .unwrap();

        assert_eq!(collected.coordinator_revision, None);
        let running: Vec<&ImpactEntry> = collected
            .input
            .entries
            .iter()
            .filter(|entry| entry.kind == KIND_RUNNING)
            .collect();
        assert_eq!(running.len(), 2, "only running groups are impact");
        assert!(running.iter().any(|entry| entry.id == "p1"));
        assert!(running.iter().any(|entry| entry.id == "p2"));
        assert!(
            running
                .iter()
                .any(|entry| entry.owner.as_deref() == Some("plugin:github")),
            "a plugin-owned group must say so"
        );
    }

    /// A left-over group whose owner is gone is exactly what a preview must not
    /// hide: it is the reason the teardown may not be clean.
    #[test]
    fn an_orphaned_group_is_still_listed() {
        let leases = LeaseTable::new();
        let records = vec![record(
            "p1",
            ProcessOwner::frontend("gone"),
            RecordState::Running,
        )];
        let collected = collect_with(
            &leases,
            &records,
            &PluginDemandTable::new(),
            CoordinatorImpact::unanswered(),
            1,
        )
        .unwrap();

        assert!(
            collected
                .input
                .entries
                .iter()
                .any(|entry| entry.id == "p1" && entry.owner.as_deref() == Some("gone"))
        );
    }

    #[test]
    fn coordinator_rows_are_included_only_when_the_daemon_answered() {
        let leases = leases_with(&[("app", LeaseMode::Business)]);
        let mut demands = PluginDemandTable::new();
        demands.acquire("app", "local-plugin");

        let known = CoordinatorImpact {
            revision: Some(9),
            running: vec![ImpactEntry {
                kind: KIND_RUNNING.to_string(),
                id: "task-1".to_string(),
                owner: Some("app".to_string()),
            }],
            queued: vec![ImpactEntry {
                kind: KIND_QUEUED.to_string(),
                id: "task-2".to_string(),
                owner: Some("app".to_string()),
            }],
            plugins: Vec::new(),
        };
        let collected = collect_with(&leases, &[], &demands, known, 1).unwrap();
        assert_eq!(collected.coordinator_revision, Some(9));
        assert!(
            collected
                .input
                .entries
                .iter()
                .any(|entry| entry.id == "task-1")
        );
        assert!(
            collected
                .input
                .entries
                .iter()
                .any(|entry| entry.id == "task-2")
        );
        // The guardian's own plugin demand is listed too.
        let plugins: Vec<&ImpactEntry> = collected
            .input
            .entries
            .iter()
            .filter(|entry| entry.kind == KIND_PLUGIN)
            .collect();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].id, "local-plugin");
    }

    /// A revision that moves mid-collection must discard the snapshot, not sign a
    /// mixture of two instants.
    #[test]
    fn a_revision_that_moves_mid_collection_is_refused() {
        let leases = leases_with(&[("app", LeaseMode::Business)]);
        let reads = Cell::new(0u64);
        let result = collect(
            &leases,
            &[],
            &PluginDemandTable::new(),
            CoordinatorImpact::unanswered(),
            true,
            || {
                reads.set(reads.get() + 1);
                reads.get()
            },
            || None,
        );
        assert_eq!(
            result.unwrap_err(),
            SnapshotRace {
                before: 1,
                after: 2
            }
        );
    }

    /// The daemon's own revision can move without the guardian's moving: a task
    /// starts or finishes, and leases are untouched. Signing the earlier rows would
    /// show an impact the user never saw.
    #[test]
    fn a_coordinator_revision_that_moves_mid_collection_is_refused() {
        let leases = leases_with(&[("app", LeaseMode::Business)]);
        let answered = CoordinatorImpact {
            revision: Some(7),
            running: Vec::new(),
            queued: Vec::new(),
            plugins: Vec::new(),
        };
        let result = collect(
            &leases,
            &[],
            &PluginDemandTable::new(),
            answered,
            true,
            || 3,
            || Some(8),
        );
        assert_eq!(
            result.unwrap_err(),
            SnapshotRace {
                before: 7,
                after: 8
            }
        );
    }

    /// A daemon that stops answering between the two reads cannot have its rows
    /// signed either: "some tasks were running" is not a state anyone can confirm.
    #[test]
    fn a_coordinator_that_falls_silent_mid_collection_is_refused() {
        let leases = leases_with(&[("app", LeaseMode::Business)]);
        let answered = CoordinatorImpact {
            revision: Some(7),
            running: Vec::new(),
            queued: Vec::new(),
            plugins: Vec::new(),
        };
        let result = collect(
            &leases,
            &[],
            &PluginDemandTable::new(),
            answered,
            true,
            || 3,
            || None,
        );
        assert!(result.is_err(), "a vanished answer must not be signed");
    }

    #[test]
    fn an_unchanged_revision_signs_the_snapshot() {
        let leases = leases_with(&[("app", LeaseMode::Business)]);
        let collected = collect(
            &leases,
            &[],
            &PluginDemandTable::new(),
            CoordinatorImpact::unanswered(),
            true,
            || 42,
            || None,
        )
        .unwrap();
        assert_eq!(collected.guardian_revision, 42);
    }

    #[test]
    fn entries_are_ordered_stably() {
        let leases = leases_with(&[
            ("zulu", LeaseMode::Business),
            ("alpha", LeaseMode::Business),
        ]);
        let first = collect_with(
            &leases,
            &[],
            &PluginDemandTable::new(),
            CoordinatorImpact::unanswered(),
            1,
        )
        .unwrap();
        let second = collect_with(
            &leases,
            &[],
            &PluginDemandTable::new(),
            CoordinatorImpact::unanswered(),
            1,
        )
        .unwrap();
        assert_eq!(first.input.entries, second.input.entries);
        let ids: Vec<&str> = first
            .input
            .entries
            .iter()
            .map(|entry| entry.id.as_str())
            .collect();
        assert_eq!(ids, vec!["alpha", "zulu"]);
    }

    #[test]
    fn fencing_requires_a_registry_and_a_handover_capable_target() {
        let mut target = ArtifactIdentity {
            build_id: "b".to_string(),
            display_version: "1".to_string(),
            executable: "/bin/true".into(),
            sha256: String::new(),
            schema_min: 0,
            schema_max: 0,
            handover_version: 1,
            business_protocol_min: 1,
            business_protocol_max: 1,
            capabilities: Vec::new(),
        };
        assert!(is_fencable(true, &target));
        assert!(!is_fencable(false, &target), "no registry means no handles");
        target.handover_version = 0;
        assert!(
            !is_fencable(true, &target),
            "a target that cannot accept a handover cannot be switched to automatically"
        );
    }
}
