//! The guardian's record of which executions belong to a frozen foreground root.
//!
//! A stop proof has two halves and neither can be faked by the other:
//!
//! * the **host** says the turn it was driving ended, and
//! * the **guardian** says every managed process group that turn started is gone.
//!
//! This module owns the second half. It answers one question -- "for this
//! root/attempt/driver epoch, are all registered tool groups observed stopped?" --
//! and it answers it by asking the real [`ProcessRegistry`], never by trusting a
//! client's claim. That is why the registration lives here rather than in the
//! daemon: a daemon able to mint its own guardian verdict would make the proof a
//! single opinion wearing two names.
//!
//! Registration is bound to the exact execution identity the daemon froze, so a
//! proof cannot be carried from one run to the next by reusing a task id.

use std::collections::BTreeMap;

use super::guardian::{ProcessRegistry, TaskExecutionKey};
use yi_agent_lifecycle::foreground::{
    EvidenceSource, ExecutionIdentity, ForegroundFence, ProcessIdentity, StopEvidence,
};

/// One registered foreground execution.
///
/// `identity` is minted by the daemon and only copied here. If the guardian invented
/// ids, a restarted daemon would mint different ones for the same work and every old
/// proof would quietly stop matching.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegisteredExecution {
    pub identity: ExecutionIdentity,
    /// The driver epoch whose turn this execution belongs to.
    ///
    /// A turn stops being authorizable the moment the driver epoch moves, so a proof
    /// taken under the old epoch must not be reused for the new one.
    pub driver_epoch: u64,
    /// The managed tool groups this execution started, as the registry names them.
    pub process_ids: Vec<String>,
}

/// The guardian's view of the executions it was told about, per root and attempt.
#[derive(Default)]
pub struct ForegroundExecutions {
    /// Keyed by (root, attempt): the same two values the daemon's fence names.
    roots: BTreeMap<(String, String), Vec<RegisteredExecution>>,
}

impl ForegroundExecutions {
    pub fn new() -> Self {
        Self::default()
    }

    fn key(fence: &ForegroundFence) -> (String, String) {
        (fence.root.root_id.clone(), fence.root.attempt_id.clone())
    }

    /// Records that one execution started under one fence.
    ///
    /// Re-registering the same execution id replaces its process list rather than
    /// appending: a second register for one identity describes the same execution
    /// twice, and two lists would make "all groups stopped" mean "all groups in
    /// whichever list was read".
    pub fn register(&mut self, fence: &ForegroundFence, execution: RegisteredExecution) {
        let entries = self.roots.entry(Self::key(fence)).or_default();
        match entries
            .iter_mut()
            .find(|seen| seen.identity.execution_id == execution.identity.execution_id)
        {
            Some(seen) => *seen = execution,
            None => entries.push(execution),
        }
    }

    /// Forgets one execution, for a turn that ended without leaving a proof.
    pub fn deregister(&mut self, fence: &ForegroundFence, execution_id: &str) {
        if let Some(entries) = self.roots.get_mut(&Self::key(fence)) {
            entries.retain(|seen| seen.identity.execution_id != execution_id);
        }
    }

    /// Every execution registered for this fence.
    pub fn registered(&self, fence: &ForegroundFence) -> Vec<RegisteredExecution> {
        self.roots
            .get(&Self::key(fence))
            .cloned()
            .unwrap_or_default()
    }

    /// What the guardian can honestly say about one frozen set.
    ///
    /// Returns the evidence it can prove and the executions it cannot. Nothing is
    /// invented:
    ///
    /// * an execution it was never told about stays blocked -- "no record" is not
    ///   "stopped", and after a restart that is exactly the state of every old turn;
    /// * a registration from another driver epoch stays blocked, because it does not
    ///   describe the driver that was frozen;
    /// * a group the registry no longer knows stays blocked, because "cannot tell"
    ///   must not be rounded up to "gone".
    ///
    /// The `joined` half is left `false` here: that fact belongs to the host and the
    /// daemon's join observer, and this function must not claim it.
    pub fn collect_evidence(
        &self,
        registry: &mut ProcessRegistry,
        fence: &ForegroundFence,
        frozen: &[ExecutionIdentity],
    ) -> (Vec<StopEvidence>, Vec<ExecutionIdentity>) {
        let registered = self.registered(fence);
        let mut evidence = Vec::new();
        let mut blocked = Vec::new();
        for execution in frozen {
            let Some(entry) = registered
                .iter()
                .find(|seen| seen.identity.execution_id == execution.execution_id)
            else {
                blocked.push(execution.clone());
                continue;
            };
            if entry.driver_epoch != fence.driver_epoch {
                blocked.push(execution.clone());
                continue;
            }
            let Some(tools_stopped) = groups_are_gone(registry, &entry.process_ids) else {
                blocked.push(execution.clone());
                continue;
            };
            evidence.push(StopEvidence {
                evidence_id: format!("guardian:{}:{}", execution.execution_id, fence.driver_epoch),
                fence: fence.clone(),
                execution: entry.identity.clone(),
                attempt_id: fence.root.attempt_id.clone(),
                source: EvidenceSource::Guardian,
                joined: false,
                tools_stopped,
                process_identity: leader_identity(registry, &entry.process_ids),
            });
        }
        (evidence, blocked)
    }
}

/// Whether every one of these registered groups is observed gone.
///
/// `None` means "cannot tell": the registry holds no record for one of the ids, which
/// happens after the journal was reclaimed or when the id came from somewhere else.
/// Rounding that to `true` would hand out a stop proof for a group nobody looked at.
fn groups_are_gone(registry: &mut ProcessRegistry, process_ids: &[String]) -> Option<bool> {
    if process_ids.is_empty() {
        // A turn that started no managed tool. The guardian's half is vacuously
        // satisfied; whether that is a proof still depends on the host's join.
        return Some(true);
    }
    let keys = registry_keys(registry, process_ids)?;
    // One attempt is the normal case. If the ids span attempts, the task scoping of
    // `task_tools_are_stopped` cannot answer for them as one set, so report blocked
    // rather than checking a subset.
    let [key] = keys.as_slice() else {
        return None;
    };
    registry.task_tools_are_stopped(key).ok()
}

/// The registry's per-attempt keys for a set of process ids.
///
/// `None` when any id is unknown to the registry. That is the *only* place "unknown"
/// is decided: an id with no record is a group nobody can vouch for, and rounding it
/// up to stopped is the leak this module exists to prevent.
fn registry_keys(
    registry: &ProcessRegistry,
    process_ids: &[String],
) -> Option<Vec<TaskExecutionKey>> {
    let mut keys: Vec<TaskExecutionKey> = Vec::new();
    for process_id in process_ids {
        let record = registry
            .records()
            .iter()
            .find(|record| &record.process_id == process_id)?;
        let execution = record.execution.clone()?;
        if !keys.contains(&execution) {
            keys.push(execution);
        }
    }
    Some(keys)
}

/// The leader's pid and kernel start time, so a recycled pid is distinguishable.
///
/// Taken from the *first* still-recorded group: a later group in the same execution
/// belongs to the same turn, and naming its leader would describe a different
/// process than the one the proof is about.
fn leader_identity(registry: &ProcessRegistry, process_ids: &[String]) -> Option<ProcessIdentity> {
    process_ids.iter().find_map(|process_id| {
        registry
            .records()
            .iter()
            .find(|record| &record.process_id == process_id)
            .map(|record| ProcessIdentity {
                pid: record.pid,
                start_micros: record.start_micros,
            })
    })
}
