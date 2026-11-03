//! Which frontend owns which root, and whether that frontend may still admit work.
//!
//! This is the store-side half of the client lease. It records, for each root,
//! which client run attached it and in which daemon generation; admission checks
//! consult it on every path that can create work. The point is that ownership
//! flows to descendants, so reclaiming one client cannot strand or steal another
//! client's tree.
//!
//! It is deliberately *not* a second lock around the coordinator. It is plain state
//! that the coordinator mutates through its existing single serialized entry
//! point, which is the only way "check the lease, then spawn" can be atomic with
//! respect to a concurrent release.

use std::collections::VecDeque;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// A root is a session/thread id in the runtime's own vocabulary.
pub type RootId = String;

/// Upper bound on remembered exited roots, so a long-lived daemon that opens and
/// closes thousands of sessions does not grow without limit.
const MAX_EXITED: usize = 4096;

/// One root's binding.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RootBinding {
    owner: String,
    generation: String,
    /// Parent root, when this root was spawned by another. Descendants inherit
    /// ownership through this chain, so a client's tree is identifiable even when
    /// the child was created by a task rather than by the frontend directly.
    parent: Option<RootId>,
}

/// A root that used to be bound and is now closed, with the reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExitedRoot {
    pub root: RootId,
    pub owner: String,
    pub generation: String,
    pub reason: ExitReason,
}

/// Why a root stopped accepting work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitReason {
    /// The client said it was leaving.
    ClientReleased,
    /// The client's lease expired without release.
    LeaseExpired,
    /// The runtime is draining for a version switch; the next generation inherits.
    Maintenance,
}

impl ExitReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClientReleased => "client_released",
            Self::LeaseExpired => "lease_expired",
            Self::Maintenance => "maintenance",
        }
    }
}

/// Client-to-root ownership and admission state for one runtime.
#[derive(Debug, Default)]
pub struct RuntimeClients {
    roots: HashMap<RootId, RootBinding>,
    /// Clients whose admission is closed. Their roots stay bound (so the UI can
    /// still explain them) but no new work may be admitted under them.
    closed: BTreeSet<String>,
    /// Roots that already stopped accepting work, oldest first.
    exited: VecDeque<ExitedRoot>,
    /// Set while a maintenance barrier is in effect.
    maintenance: Option<u64>,
    /// Set once the last client left and the whole scope is draining.
    ///
    /// Distinct from [`Self::closed`] (which is per client) and from `maintenance`
    /// (which is a version switch): a global drain means "this generation is
    /// finishing, send newcomers to the next one". Conflating them would let a
    /// version switch look like an ordinary client exit.
    global_drain: bool,
}

impl RuntimeClients {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind a root to the client that attached it.
    ///
    /// Refuses to rebind a live root to a different client: a second frontend
    /// must not be able to take over a running session by guessing its thread id.
    pub fn bind_root(&mut self, client: &str, root: &str, generation: &str) -> Result<(), String> {
        self.bind_root_with_parent(client, root, generation, None)
    }

    /// Bind a descendant root, inheriting ownership from its parent.
    pub fn bind_child_root(
        &mut self,
        parent: &str,
        root: &str,
        generation: &str,
    ) -> Result<(), String> {
        let owner = self
            .roots
            .get(parent)
            .map(|binding| binding.owner.clone())
            .ok_or_else(|| format!("parent root {parent} is not bound"))?;
        self.bind_root_with_parent(&owner, root, generation, Some(parent.to_string()))
    }

    /// Records a descendant's ownership under its parent's, *without* consulting
    /// the client's closed state.
    ///
    /// A child admitted just before its client released is still that client's
    /// work, and reclamation has to find it. Recording that fact grants no new
    /// authority: admission is decided by [`Self::root_is_open`], which still
    /// refuses everything under a closed owner. Refusing the bookkeeping instead
    /// would leave admitted work that no one can enumerate and reclaim.
    pub fn adopt_descendant(
        &mut self,
        parent: &str,
        root: &str,
        generation: &str,
    ) -> Result<(), String> {
        let owner = self
            .roots
            .get(parent)
            .map(|binding| binding.owner.clone())
            .ok_or_else(|| format!("parent root {parent} is not bound"))?;
        if self.is_exited(root) {
            return Err(format!(
                "root {root} already stopped accepting work; it cannot be adopted"
            ));
        }
        if let Some(existing) = self.roots.get(root) {
            if existing.owner != owner {
                return Err(format!(
                    "root {root} is owned by client {}; client {owner} cannot take it over",
                    existing.owner
                ));
            }
            return Ok(());
        }
        self.roots.insert(
            root.to_string(),
            RootBinding {
                owner,
                generation: generation.to_string(),
                parent: Some(parent.to_string()),
            },
        );
        Ok(())
    }

    fn bind_root_with_parent(
        &mut self,
        client: &str,
        root: &str,
        generation: &str,
        parent: Option<RootId>,
    ) -> Result<(), String> {
        if self.is_exited(root) {
            return Err(format!(
                "root {root} already stopped accepting work; it cannot be rebound"
            ));
        }
        if let Some(existing) = self.roots.get(root) {
            if existing.owner != client {
                return Err(format!(
                    "root {root} is owned by client {}; client {client} cannot take it over",
                    existing.owner
                ));
            }
            if existing.generation != generation {
                return Err(format!(
                    "root {root} is bound to generation {}; a new generation must rebind explicitly",
                    existing.generation
                ));
            }
            return Ok(());
        }
        if self.closed.contains(client) {
            return Err(format!("client {client} has closed admission"));
        }
        self.roots.insert(
            root.to_string(),
            RootBinding {
                owner: client.to_string(),
                generation: generation.to_string(),
                parent,
            },
        );
        Ok(())
    }

    /// May this client admit work under this root in this generation?
    pub fn authorize(&self, client: &str, root: &str, generation: &str) -> Result<(), String> {
        if self.global_drain {
            return Err("this runtime generation is draining; wait for the next one".to_string());
        }
        if self.closed.contains(client) {
            return Err(format!("client {client} has closed admission"));
        }
        if let Some(&revision) = self.maintenance.as_ref() {
            return Err(format!(
                "a maintenance barrier (revision {revision}) is in effect; \
                 existing leases only"
            ));
        }
        let binding = self
            .roots
            .get(root)
            .ok_or_else(|| format!("root {root} is not bound to any client"))?;
        if binding.owner != client {
            return Err(format!(
                "root {root} is owned by client {}, not {client}",
                binding.owner
            ));
        }
        if binding.generation != generation {
            return Err(format!(
                "root {root} belongs to generation {}, not {generation}",
                binding.generation
            ));
        }
        Ok(())
    }

    /// Close a client's admission, returning the roots it owned.
    ///
    /// Idempotent: a second call returns the same roots and changes nothing. The
    /// roots are *not* unbound — an in-flight task still needs to explain itself —
    /// but nothing new may be admitted under them, and ownership of descendants is
    /// cut by the same generation check.
    pub fn close_client_admission(&mut self, client: &str) -> Vec<RootId> {
        self.closed.insert(client.to_string());
        let mut roots: Vec<RootId> = self
            .roots
            .iter()
            .filter(|(_, binding)| binding.owner == client)
            .map(|(root, _)| root.clone())
            .collect();
        roots.sort();
        // Record the exit once; a repeated close must not duplicate history.
        for root in &roots {
            if !self.is_exited(root)
                && let Some(binding) = self.roots.get(root)
            {
                self.push_exited(ExitedRoot {
                    root: root.clone(),
                    owner: binding.owner.clone(),
                    generation: binding.generation.clone(),
                    reason: ExitReason::ClientReleased,
                });
            }
        }
        roots
    }

    /// Start a maintenance barrier.
    ///
    /// `expected_coordinator_revision` is the revision the caller believes it is
    /// acting on; a mismatch means someone else changed the state in between and
    /// the barrier must not be installed on stale assumptions.
    pub fn begin_maintenance(&mut self, expected_coordinator_revision: u64) -> Result<(), String> {
        if let Some(current) = self.maintenance {
            if current == expected_coordinator_revision {
                return Ok(());
            }
            return Err(format!(
                "a maintenance barrier is already active at revision {current}, \
                 not {expected_coordinator_revision}"
            ));
        }
        self.maintenance = Some(expected_coordinator_revision);
        Ok(())
    }

    /// Lift the barrier once the new generation is serving.
    pub fn end_maintenance(&mut self) {
        self.maintenance = None;
    }

    pub fn in_maintenance(&self) -> bool {
        self.maintenance.is_some()
    }

    /// A task's execution is physically over. Records the exit; rejects a stale
    /// generation or an attempt that is not the one currently bound.
    ///
    /// The caller must have independent evidence of the stop. A worker reporting
    /// `Completed` is not evidence: the event says the *agent loop* finished, not
    /// that the thread and its children are gone.
    pub fn execution_stopped(
        &mut self,
        task: &str,
        attempt: &str,
        generation: &str,
    ) -> Result<(), String> {
        if attempt.is_empty() {
            return Err("an attempt id is required to confirm a stop".into());
        }
        if let Some(exited) = self.exited.iter().find(|exited| exited.root == task) {
            if exited.generation != generation {
                return Err(format!(
                    "task {task} already stopped in generation {}; \
                     a stop from generation {generation} is stale",
                    exited.generation
                ));
            }
            return Err(format!("task {task} was already confirmed stopped"));
        }
        let binding = self
            .roots
            .get(task)
            .ok_or_else(|| format!("task {task} is not bound to any root"))?;
        if binding.generation != generation {
            return Err(format!(
                "task {task} belongs to generation {}, not {generation}",
                binding.generation
            ));
        }
        let owner = binding.owner.clone();
        self.push_exited(ExitedRoot {
            root: task.to_string(),
            owner,
            generation: generation.to_string(),
            reason: ExitReason::ClientReleased,
        });
        Ok(())
    }

    /// Whether a root may still admit new work, whatever client is asking.
    ///
    /// This is deliberately not "does *this* client own it": the question a new
    /// task入口 must answer is "is this tree still open at all". The caller's
    /// identity is checked separately, by [`Self::authorize`], using an id the
    /// guardian verified rather than one the business RPC supplied.
    pub fn root_is_open(&self, root: &str, generation: &str) -> Result<(), String> {
        if self.global_drain {
            return Err("this runtime generation is draining; wait for the next one".to_string());
        }
        if self.is_exited(root) {
            return Err(format!("root {root} has stopped accepting work"));
        }
        if let Some(&revision) = self.maintenance.as_ref() {
            return Err(format!(
                "a maintenance barrier (revision {revision}) is in effect;                  existing leases only"
            ));
        }
        match self.roots.get(root) {
            // A close cuts the whole tree, including descendants bound before it.
            Some(binding) if self.closed.contains(&binding.owner) => Err(format!(
                "root {root} belongs to client {} whose admission is closed",
                binding.owner
            )),
            Some(binding) if binding.generation != generation => Err(format!(
                "root {root} belongs to generation {}, not {generation}",
                binding.generation
            )),
            // An unbound root is a session that has not been attached yet; the
            // bind at attach time is what closes this hole.
            _ => Ok(()),
        }
    }

    /// The client that owns a bound root, if any.
    pub fn owner_of(&self, root: &str) -> Option<String> {
        self.roots.get(root).map(|binding| binding.owner.clone())
    }

    /// Roots a client currently owns, sorted.
    pub fn roots_of(&self, client: &str) -> Vec<RootId> {
        let mut roots: Vec<RootId> = self
            .roots
            .iter()
            .filter(|(_, binding)| binding.owner == client)
            .map(|(root, _)| root.clone())
            .collect();
        roots.sort();
        roots
    }

    /// Whether a client's admission is closed.
    pub fn is_closed(&self, client: &str) -> bool {
        self.closed.contains(client)
    }

    /// Refuse all new work for this generation because its last client left.
    ///
    /// Idempotent, and it does not unbind anything: a task still running has to be
    /// able to explain itself. It exists so a newcomer can be told to wait for the
    /// next generation instead of being admitted into a runtime that is going away.
    pub fn begin_global_drain(&mut self) {
        self.global_drain = true;
    }

    /// Whether this generation stopped admitting work entirely.
    pub fn in_global_drain(&self) -> bool {
        self.global_drain
    }

    /// How many clients still have an open admission.
    ///
    /// Counted from bound roots rather than from a separate roster: a client that
    /// never bound anything has no work here, and it must not keep a generation
    /// alive after everyone who *does* have work has left.
    pub fn open_clients(&self) -> usize {
        let mut owners: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for binding in self.roots.values() {
            if !self.closed.contains(&binding.owner) {
                owners.insert(binding.owner.as_str());
            }
        }
        owners.len()
    }

    /// Bound roots, for persistence and diagnostics.
    pub fn bound_roots(&self) -> BTreeMap<RootId, String> {
        self.roots
            .iter()
            .map(|(root, binding)| (root.clone(), binding.owner.clone()))
            .collect()
    }

    /// Recently exited roots, newest last.
    pub fn exited(&self) -> &VecDeque<ExitedRoot> {
        &self.exited
    }

    pub fn active_count(&self) -> usize {
        self.roots.len()
    }

    fn is_exited(&self, root: &str) -> bool {
        self.exited.iter().any(|exited| exited.root == root)
    }

    fn push_exited(&mut self, exited: ExitedRoot) {
        if self.exited.len() >= MAX_EXITED {
            self.exited.pop_front();
        }
        self.exited.push_back(exited);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_child_inherits_its_parents_owner() {
        let mut clients = RuntimeClients::new();
        clients.bind_root("app", "thread:a", "g1").unwrap();
        clients.bind_child_root("thread:a", "task:1", "g1").unwrap();
        assert!(clients.authorize("app", "task:1", "g1").is_ok());
        // A different client cannot admit work under someone else's descendant.
        assert!(clients.authorize("tui", "task:1", "g1").is_err());
    }

    #[test]
    fn a_child_of_an_unbound_parent_is_refused() {
        let mut clients = RuntimeClients::new();
        assert!(
            clients
                .bind_child_root("thread:none", "task:1", "g1")
                .is_err()
        );
    }

    #[test]
    fn closing_twice_returns_the_same_roots_without_duplicating_history() {
        let mut clients = RuntimeClients::new();
        clients.bind_root("app", "thread:a", "g1").unwrap();
        let first = clients.close_client_admission("app");
        let second = clients.close_client_admission("app");
        assert_eq!(first, second);
        assert_eq!(clients.exited().len(), 1);
    }

    #[test]
    fn a_maintenance_barrier_blocks_new_admission() {
        let mut clients = RuntimeClients::new();
        clients.bind_root("app", "thread:a", "g1").unwrap();
        clients.begin_maintenance(7).unwrap();
        assert!(clients.authorize("app", "thread:a", "g1").is_err());
        // Installing the same barrier twice is idempotent, a different revision is not.
        assert!(clients.begin_maintenance(7).is_ok());
        assert!(clients.begin_maintenance(8).is_err());
        clients.end_maintenance();
        assert!(clients.authorize("app", "thread:a", "g1").is_ok());
    }

    #[test]
    fn a_stop_from_an_old_generation_is_stale() {
        let mut clients = RuntimeClients::new();
        clients.bind_root("app", "thread:a", "g1").unwrap();
        clients
            .execution_stopped("thread:a", "attempt-1", "g0")
            .unwrap_err();
        clients
            .execution_stopped("thread:a", "attempt-1", "g1")
            .unwrap();
        // Releasing twice, or from a newer generation, is refused.
        clients
            .execution_stopped("thread:a", "attempt-1", "g1")
            .unwrap_err();
        clients
            .execution_stopped("thread:a", "attempt-2", "g2")
            .unwrap_err();
    }

    #[test]
    fn an_empty_attempt_is_refused() {
        let mut clients = RuntimeClients::new();
        clients.bind_root("app", "thread:a", "g1").unwrap();
        assert!(clients.execution_stopped("thread:a", "", "g1").is_err());
    }

    #[test]
    fn a_global_drain_refuses_every_client_not_only_the_departing_one() {
        // A project-wide drain is not "this client left": a TUI still present must
        // also stop admitting work, because the generation it would run in is gone.
        let mut clients = RuntimeClients::new();
        clients.bind_root("app", "thread:a", "g1").unwrap();
        clients.bind_root("tui", "thread:t", "g1").unwrap();
        clients.begin_global_drain();
        assert!(clients.authorize("app", "thread:a", "g1").is_err());
        assert!(clients.authorize("tui", "thread:t", "g1").is_err());
        assert!(clients.root_is_open("thread:t", "g1").is_err());
    }

    #[test]
    fn one_client_leaving_does_not_start_a_global_drain() {
        let mut clients = RuntimeClients::new();
        clients.bind_root("app", "thread:a", "g1").unwrap();
        clients.bind_root("tui", "thread:t", "g1").unwrap();
        clients.close_client_admission("app");
        assert!(!clients.in_global_drain());
        assert!(clients.authorize("tui", "thread:t", "g1").is_ok());
    }

    #[test]
    fn open_clients_counts_only_those_still_admitting() {
        let mut clients = RuntimeClients::new();
        clients.bind_root("app", "thread:a", "g1").unwrap();
        clients.bind_root("tui", "thread:t", "g1").unwrap();
        assert_eq!(clients.open_clients(), 2);
        clients.close_client_admission("app");
        assert_eq!(clients.open_clients(), 1);
        clients.close_client_admission("tui");
        assert_eq!(clients.open_clients(), 0);
    }

    #[test]
    fn an_exited_root_cannot_be_rebound() {
        let mut clients = RuntimeClients::new();
        clients.bind_root("app", "thread:a", "g1").unwrap();
        clients
            .execution_stopped("thread:a", "attempt-1", "g1")
            .unwrap();
        assert!(clients.bind_root("app", "thread:a", "g1").is_err());
        assert!(clients.bind_root("tui", "thread:a", "g2").is_err());
    }
}

/// Persistence for the ownership record.
///
/// Ownership itself lives in memory because it is per daemon generation; this table
/// answers the questions that must survive a restart: which client attached a given
/// session last, and why it ended. It also lets a restart distinguish "never bound"
/// from "bound and closed", so a stale thread id from a previous generation is not
/// mistaken for a fresh one.
pub mod store {
    use super::{ExitReason, RootId, RuntimeClients};
    use crate::repository::{RepositoryError, RuntimeRepository};

    /// Record (or refresh) one root's current owner.
    pub fn record_binding(
        repository: &RuntimeRepository,
        root: &RootId,
        owner: &str,
        generation: &str,
    ) -> Result<(), RepositoryError> {
        repository.record_root_owner(root, owner, generation, None)
    }

    /// Record that a root stopped accepting work.
    ///
    /// A second write for the same root only fills in the reason if it was absent:
    /// the first terminal event is the true one, and overwriting it would erase the
    /// distinction between "the client left" and "maintenance drained it".
    pub fn record_exit(
        repository: &RuntimeRepository,
        root: &RootId,
        reason: ExitReason,
    ) -> Result<(), RepositoryError> {
        repository.mark_root_exited(root, reason.as_str())
    }

    /// Write the current in-memory ownership of every client that is closing.
    pub fn record_client_close(
        repository: &RuntimeRepository,
        clients: &RuntimeClients,
        client: &str,
        generation: &str,
        reason: ExitReason,
    ) -> Result<Vec<RootId>, RepositoryError> {
        let roots = clients.roots_of(client);
        for root in &roots {
            record_binding(repository, root, client, generation)?;
            record_exit(repository, root, reason)?;
        }
        Ok(roots)
    }
}
