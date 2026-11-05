//! Session delegation: identity authentication, binding CAS and per-generation
//! turn fences.
//!
//! Three things are deliberately kept apart here, because conflating them was the
//! original bug:
//!
//! * The **role** says whether a task is a container at all. It never changes.
//! * The **binding** says which host may act for the container in this daemon
//!   generation, and bumps only when the owner genuinely changes.
//! * The **turn** is a short-lived fence for one run. Ending it closes writes but
//!   does not cancel the children the session already owns.
//!
//! Nothing in this module reads `TaskState` to decide admission. A container whose
//! last execution ended in `stalled` is still open unless its own tombstone says
//! otherwise.

use std::collections::HashMap;
use std::sync::Mutex;

use sha2::{Digest, Sha256};
use yi_agent_core::subagent::task::{TaskId, TaskRole};
use yi_agent_lifecycle::foreground::SecretCapability;
use yi_agent_lifecycle::session_delegation::{
    PrepareSession, SessionBinding, SessionFailureCode, SessionFence, SessionIdentity, SessionTurn,
    session_resume_digest,
};

use crate::repository::SessionBindingCommit;
use crate::runtime::{RuntimeCoordinator, RuntimeCoordinatorError};

/// Whether a presented turn is the open turn.
///
/// Pure so the rule can be pinned without a database and reused by every write
/// route. A stale fence is reported before "closed" because a caller holding an
/// old generation must re-prepare, not retry: telling it the turn merely closed
/// would invite a retry that can never work.
pub(crate) fn validate_turn(
    current: &SessionTurn,
    presented: &SessionTurn,
    open: bool,
) -> Result<(), SessionFailureCode> {
    if current.fence != presented.fence {
        return Err(SessionFailureCode::StaleBinding);
    }
    if current.turn_id != presented.turn_id {
        return Err(SessionFailureCode::StaleTurn);
    }
    if current.capability != presented.capability {
        return Err(SessionFailureCode::AuthorityDenied);
    }
    if !open {
        return Err(SessionFailureCode::AdmissionClosed);
    }
    Ok(())
}

/// Whether the old host may be replaced.
///
/// A join without the tool groups gone means a process may still be writing; tool
/// groups gone without the join means the execution itself is unaccounted for.
/// Either half alone is not a stop proof, so both are required.
pub(crate) fn may_replace_driver(joined: bool, tools_stopped: bool) -> bool {
    joined && tools_stopped
}

/// One open turn, held only in memory.
///
/// A turn is a live fence: persisting its secret would let a restarted daemon
/// honour a capability the user's tools still hold. Keeping it here means a
/// restart closes every turn, which is what "the daemon that issued it is gone"
/// should mean.
#[derive(Clone, Debug)]
struct OpenTurn {
    turn: SessionTurn,
    /// Whether new writes may use this turn. A closed turn is kept, not dropped:
    /// the run that held it may still need to read the results of the children it
    /// started, and dropping the secret would turn "the turn ended" into "the
    /// tools' own history became unreadable".
    open: bool,
}

/// Live bindings and turns for session containers.
#[derive(Default)]
pub(crate) struct SessionDelegationState {
    /// The binding secret per root. Never persisted; reissued by `prepare`.
    bindings: HashMap<String, SessionBinding>,
    /// Which turn is open per root.
    turns: HashMap<String, OpenTurn>,
}

pub(crate) type State = Mutex<SessionDelegationState>;

/// The result of attaching a session container: an address, never a secret.
pub struct AttachedSessionContainer {
    pub root: yi_agent_lifecycle::session_delegation::SessionRef,
    pub workspace: yi_agent_core::subagent::worker::WorkerWorkspace,
}

impl AttachedSessionContainer {
    pub fn reference(&self) -> yi_agent_lifecycle::session_delegation::SessionRef {
        self.root.clone()
    }
}

/// One execution registration, before the daemon mints its identity.
pub struct RegisterExecution {
    pub root_task_id: TaskId,
    pub fence: SessionFence,
    pub turn_id: String,
}

impl RuntimeCoordinatorError {
    /// One refusal, built from a stable lifecycle code plus a reason.
    pub(crate) fn session_delegation(code: SessionFailureCode, reason: impl Into<String>) -> Self {
        Self::SessionDelegation {
            code,
            reason: reason.into(),
        }
    }
}

impl RuntimeCoordinator {
    /// (Re)establishes the host binding for a container.
    ///
    /// The order is deliberate: authenticate the identity, check the tombstone,
    /// then either reuse the caller's still-valid binding or take a new epoch.
    /// A caller that merely repeats `host_run_id` does not get the secret back —
    /// only a binding it already holds is reused, because a run id is not proof.
    pub fn prepare_session(
        &self,
        request: PrepareSession,
    ) -> Result<SessionBinding, RuntimeCoordinatorError> {
        self.ensure_admitting()?;
        let root: TaskId = request.root.root_id.parse().map_err(|_| {
            RuntimeCoordinatorError::session_delegation(
                SessionFailureCode::CorruptAttachment,
                format!("{} is not a task id", request.root.root_id),
            )
        })?;
        let (role, record) = {
            let repository = self
                .repository
                .lock()
                .expect("runtime repository mutex poisoned");
            repository.session_prepare_facts(&request)?
        };
        if role != TaskRole::SessionContainer {
            // An execution may not borrow the container identity. Without this a
            // settled child could re-open delegation on behalf of a session it was
            // only ever a leaf of.
            return Err(RuntimeCoordinatorError::SessionDelegation {
                code: SessionFailureCode::AuthorityDenied,
                reason: format!("{} is not a session container", request.root.root_id),
            });
        }
        if record.closed {
            return Err(RuntimeCoordinatorError::SessionDelegation {
                code: SessionFailureCode::SessionClosed,
                reason: format!("session {} is closed", request.root.root_id),
            });
        }
        self.authenticate_session_identity(&root, &record, &request)?;

        if let Some(current) = request.current_binding.as_ref()
            && current.fence.generation == self.generation
            && current.fence.binding_epoch == record.binding_epoch
            && current.fence.driver_epoch == record.driver_epoch
            && record.owner_run_id.as_deref() == Some(request.host_run_id.as_str())
        {
            // Still the same live owner in the same generation: reuse, do not bump.
            // A new epoch per turn would make every turn look like a driver
            // replacement and force a stop proof that has nothing to prove.
            self.session_delegation_state()
                .lock()
                .expect("session delegation mutex poisoned")
                .bindings
                .insert(request.root.root_id.clone(), current.clone());
            return Ok(current.clone());
        }

        // Everything below replaces a driver, so the old driver's executions must
        // already be retired. This read is outside any lock that awaits: it is a
        // local snapshot, re-checked by the transaction's own fence clauses.
        let (unsettled, retired) = self.settle_replaced_driver(&root, &request)?;
        if !unsettled.is_empty() {
            return Err(RuntimeCoordinatorError::session_delegation(
                SessionFailureCode::StopUnproven,
                format!(
                    "{} execution(s) of the previous driver are not proven stopped;                      a missing record is not a stop proof",
                    unsettled.len()
                ),
            ));
        }

        let digest = request_digest(&request);
        let commit = SessionBindingCommit {
            root: request.root.clone(),
            expected_binding_epoch: record.binding_epoch,
            operation_id: request.operation_id.clone(),
            request_digest: digest,
            owner_run_id: request.host_run_id.clone(),
            generation: self.generation.clone(),
            resume_digest: request.replacement_resume_digest.clone(),
            consume_legacy_upgrade: matches!(request.identity, SessionIdentity::LegacyUpgrade(_)),
            retired_executions: retired,
        };
        let committed = self
            .repository
            .lock()
            .expect("runtime repository mutex poisoned")
            .commit_session_binding(&commit)?;
        let binding = SessionBinding {
            fence: SessionFence {
                root: request.root.clone(),
                generation: self.generation.clone(),
                binding_epoch: committed.binding_epoch,
                driver_epoch: committed.driver_epoch,
            },
            capability: SecretCapability::new(self.new_session_secret()),
        };
        self.session_delegation_state()
            .lock()
            .expect("session delegation mutex poisoned")
            .bindings
            .insert(request.root.root_id.clone(), binding.clone());
        Ok(binding)
    }

    /// Attaches a session container and, at creation only, records its resume digest.
    ///
    /// Reusing the application attach keeps one place that decides ownership,
    /// workspace and role. What this adds is the create-only digest: an existing
    /// container is returned as an address and nothing else, so a second caller
    /// cannot install a secret the live host does not know.
    pub async fn attach_session_container(
        &self,
        idempotency_key: &str,
        workspace: &std::path::Path,
        extra_roots: &[String],
        operation_id: &str,
        creation_resume_digest: Option<String>,
        _identity: Option<yi_agent_lifecycle::session_delegation::SessionIdentity>,
    ) -> Result<AttachedSessionContainer, RuntimeCoordinatorError> {
        if operation_id.trim().is_empty() {
            return Err(RuntimeCoordinatorError::session_delegation(
                SessionFailureCode::CorruptAttachment,
                "an attach needs a stable operation id",
            ));
        }
        let attached = self
            .attach_application_root(idempotency_key, workspace, extra_roots)
            .await?;
        if let Some(digest) = creation_resume_digest {
            if digest.trim().is_empty() {
                return Err(RuntimeCoordinatorError::session_delegation(
                    SessionFailureCode::SessionIdentityRequired,
                    "a resume digest may not be empty",
                ));
            }
            self.repository
                .lock()
                .expect("runtime repository mutex poisoned")
                .set_session_resume_digest(&attached.root_task_id, &digest)?;
        }
        Ok(AttachedSessionContainer {
            root: yi_agent_lifecycle::session_delegation::SessionRef {
                session_id: attached.session_id.to_string(),
                root_id: attached.root_task_id.to_string(),
            },
            workspace: attached.workspace,
        })
    }

    /// Registers one execution of a bound driver before it starts.
    ///
    ///
    /// Written before the turn can touch anything: an execution the daemon has no
    /// record of cannot be proven stopped later, so the row has to exist first.
    pub fn record_host_execution(
        &self,
        request: crate::runtime::session_delegation::RegisterExecution,
    ) -> Result<yi_agent_lifecycle::foreground::ExecutionIdentity, RuntimeCoordinatorError> {
        let RegisterExecution {
            root_task_id,
            fence,
            turn_id,
        } = request;
        if fence.generation != self.generation {
            return Err(RuntimeCoordinatorError::SessionDelegation {
                code: SessionFailureCode::StaleGeneration,
                reason: "the execution belongs to an older daemon generation".to_string(),
            });
        }
        let execution = yi_agent_lifecycle::foreground::ExecutionIdentity {
            execution_id: format!("exec-{}", uuid::Uuid::new_v4()),
            task_id: root_task_id.to_string(),
            attempt_id: turn_id.clone(),
            kind: yi_agent_lifecycle::foreground::ExecutionKind::Driver,
        };
        let row = crate::repository::SessionHostExecution {
            execution_id: execution.execution_id.clone(),
            root_task_id: root_task_id.to_string(),
            generation: fence.generation.clone(),
            binding_epoch: fence.binding_epoch,
            driver_epoch: fence.driver_epoch,
            turn_id,
            identity_json: serde_json::to_string(&execution).map_err(|error| {
                RuntimeCoordinatorError::SessionDelegation {
                    code: SessionFailureCode::CorruptAttachment,
                    reason: format!("an execution identity is unserialisable: {error}"),
                }
            })?,
            phase: "starting".to_string(),
            evidence_json: None,
            execution_set_version: 0,
        };
        self.repository
            .lock()
            .expect("runtime repository mutex poisoned")
            .register_session_host_execution(&row)?;
        Ok(execution)
    }

    /// Retires one execution with evidence the daemon itself can verify.
    ///
    /// The fence is part of the match, so a host cannot retire another driver's
    /// execution by naming its id.
    pub fn record_host_execution_stop(
        &self,
        fence: &SessionFence,
        evidence: &yi_agent_lifecycle::foreground::StopEvidence,
    ) -> Result<(), RuntimeCoordinatorError> {
        if !may_replace_driver(evidence.joined, evidence.tools_stopped) {
            return Err(RuntimeCoordinatorError::SessionDelegation {
                code: SessionFailureCode::StopUnproven,
                reason: "a stop needs both the join and the tool groups gone".to_string(),
            });
        }
        if evidence.fence.generation != fence.generation
            || evidence.fence.binding_epoch != fence.binding_epoch
            || evidence.fence.driver_epoch != fence.driver_epoch
            || evidence.fence.root.root_id != fence.root.root_id
        {
            return Err(RuntimeCoordinatorError::SessionDelegation {
                code: SessionFailureCode::StaleBinding,
                reason: "the evidence describes another fence".to_string(),
            });
        }
        let evidence_json = serde_json::to_string(evidence).map_err(|error| {
            RuntimeCoordinatorError::SessionDelegation {
                code: SessionFailureCode::CorruptAttachment,
                reason: format!("a stop proof is unserialisable: {error}"),
            }
        })?;
        self.repository
            .lock()
            .expect("runtime repository mutex poisoned")
            .record_session_host_stop(&crate::repository::SessionHostStop {
                execution_id: evidence.execution.execution_id.clone(),
                root: fence.root.root_id.clone(),
                generation: fence.generation.clone(),
                binding_epoch: fence.binding_epoch,
                driver_epoch: fence.driver_epoch,
                evidence_json,
                execution_set_version: 0,
            })?;
        Ok(())
    }

    /// Opens one turn for a bound host.
    ///
    /// A second turn for the same root replaces the first only after the first was
    /// ended: the previous turn's tools must not keep working while a new turn is
    /// open, or an abandoned run could still spawn.
    pub fn begin_session_turn(
        &self,
        binding: &SessionBinding,
        turn_id: &str,
    ) -> Result<SessionTurn, RuntimeCoordinatorError> {
        self.ensure_admitting()?;
        if binding.fence.generation != self.generation {
            return Err(RuntimeCoordinatorError::SessionDelegation {
                code: SessionFailureCode::StaleGeneration,
                reason: format!(
                    "binding generation {} is not {}",
                    binding.fence.generation, self.generation
                ),
            });
        }
        let mut state = self
            .session_delegation_state()
            .lock()
            .expect("session delegation mutex poisoned");
        let Some(live) = state.bindings.get(&binding.fence.root.root_id) else {
            return Err(RuntimeCoordinatorError::SessionDelegation {
                code: SessionFailureCode::StaleBinding,
                reason: format!("{} has no live binding", binding.fence.root.root_id),
            });
        };
        if live != binding {
            return Err(RuntimeCoordinatorError::SessionDelegation {
                code: SessionFailureCode::StaleBinding,
                reason: "the presented binding is not the live one".to_string(),
            });
        }
        let turn = SessionTurn {
            fence: binding.fence.clone(),
            turn_id: turn_id.to_string(),
            capability: SecretCapability::new(self.new_session_secret()),
        };
        state.turns.insert(
            binding.fence.root.root_id.clone(),
            OpenTurn {
                turn: turn.clone(),
                open: true,
            },
        );
        Ok(turn)
    }

    /// Closes one turn. Idempotent for the matching turn; an old turn may not close
    /// a newer one.
    pub fn end_session_turn(&self, turn: &SessionTurn) -> Result<(), RuntimeCoordinatorError> {
        let mut state = self
            .session_delegation_state()
            .lock()
            .expect("session delegation mutex poisoned");
        let Some(open) = state.turns.get_mut(&turn.fence.root.root_id) else {
            return Ok(());
        };
        if open.turn.fence == turn.fence && open.turn.turn_id == turn.turn_id {
            // Kept, not removed: a closed turn is still a secret this daemon
            // minted, and reads of the children it started must keep working.
            // Only writes stop, which `authorize_session_write` decides.
            open.open = false;
        }
        Ok(())
    }

    /// Verifies a control stream may act for this container.
    ///
    /// The binding capability never leaves the host, so a caller holding one is
    /// that host in this generation. Equality against the live binding is the
    /// whole check: a turn secret is not a binding, and cannot get here.
    pub fn open_host_control(
        &self,
        binding: &SessionBinding,
    ) -> Result<(), RuntimeCoordinatorError> {
        if binding.fence.generation != self.generation {
            return Err(RuntimeCoordinatorError::SessionDelegation {
                code: SessionFailureCode::StaleGeneration,
                reason: "the control stream belongs to an older daemon generation".to_string(),
            });
        }
        let state = self
            .session_delegation_state()
            .lock()
            .expect("session delegation mutex poisoned");
        let Some(live) = state.bindings.get(&binding.fence.root.root_id) else {
            return Err(RuntimeCoordinatorError::SessionDelegation {
                code: SessionFailureCode::StaleBinding,
                reason: "this container has no live binding".to_string(),
            });
        };
        if live != binding {
            // A turn secret is the one capability a tool holds, so it is worth
            // naming explicitly rather than reporting the generic "not the live
            // binding": the host needs to know a tool tried to open the control
            // stream, not that its own binding looked odd.
            let turn_secret = state
                .turns
                .get(&binding.fence.root.root_id)
                .is_some_and(|open| open.turn.capability == binding.capability);
            return Err(RuntimeCoordinatorError::SessionDelegation {
                code: if turn_secret {
                    SessionFailureCode::AuthorityDenied
                } else {
                    SessionFailureCode::StaleBinding
                },
                reason: if turn_secret {
                    "a turn secret cannot open the host control stream".to_string()
                } else {
                    "the presenting binding is not the live one".to_string()
                },
            });
        }
        Ok(())
    }

    /// Registers one execution of a bound host's current turn.
    ///
    /// The turn has to be the open one: registering after a later turn opened
    /// would let an abandoned run leave a `starting` row that blocks every
    /// future driver for no reason.
    pub fn register_host_control_execution(
        &self,
        binding: &SessionBinding,
        turn_id: &str,
    ) -> Result<yi_agent_lifecycle::foreground::ExecutionIdentity, RuntimeCoordinatorError> {
        self.open_host_control(binding)?;
        {
            let state = self
                .session_delegation_state()
                .lock()
                .expect("session delegation mutex poisoned");
            let Some(open) = state.turns.get(&binding.fence.root.root_id) else {
                return Err(RuntimeCoordinatorError::SessionDelegation {
                    code: SessionFailureCode::AdmissionClosed,
                    reason: "no turn is open for this session".to_string(),
                });
            };
            if open.turn.turn_id != turn_id || open.turn.fence != binding.fence {
                return Err(RuntimeCoordinatorError::SessionDelegation {
                    code: SessionFailureCode::StaleTurn,
                    reason: "the execution does not name the open turn".to_string(),
                });
            }
        }
        let root: TaskId = binding.fence.root.root_id.parse().map_err(|_| {
            RuntimeCoordinatorError::session_delegation(
                SessionFailureCode::CorruptAttachment,
                format!("{} is not a task id", binding.fence.root.root_id),
            )
        })?;
        self.record_host_execution(RegisterExecution {
            root_task_id: root,
            fence: binding.fence.clone(),
            turn_id: turn_id.to_string(),
        })
    }

    /// Records a stop proof on the host control stream.
    ///
    /// The evidence must describe this host's own fence; the repository's
    /// conditional update then refuses an execution id that is not live under
    /// that fence, so naming someone else's execution cannot retire it.
    pub fn record_host_control_stop(
        &self,
        binding: &SessionBinding,
        evidence: &yi_agent_lifecycle::foreground::StopEvidence,
    ) -> Result<(), RuntimeCoordinatorError> {
        self.open_host_control(binding)?;
        self.record_host_execution_stop(&binding.fence, evidence)
    }

    /// Pre-checks that a write may proceed under this turn.
    ///
    /// A pre-check only. A caller must re-check inside the gate that performs the
    /// side effect; a returning `Ok` here is not a permanent permit.
    pub fn authorize_session_write(
        &self,
        turn: &SessionTurn,
    ) -> Result<(), RuntimeCoordinatorError> {
        if turn.fence.generation != self.generation {
            return Err(RuntimeCoordinatorError::SessionDelegation {
                code: SessionFailureCode::StaleGeneration,
                reason: "the turn belongs to an older daemon generation".to_string(),
            });
        }
        let state = self
            .session_delegation_state()
            .lock()
            .expect("session delegation mutex poisoned");
        let Some(open) = state.turns.get(&turn.fence.root.root_id) else {
            return Err(RuntimeCoordinatorError::SessionDelegation {
                code: SessionFailureCode::AdmissionClosed,
                reason: "no turn is open for this session".to_string(),
            });
        };
        validate_turn(&open.turn, turn, open.open).map_err(|code| {
            RuntimeCoordinatorError::session_delegation(
                code,
                "the presented turn is not the open one".to_string(),
            )
        })
    }

    /// Whether `capability` is a turn secret this daemon issued for this container.
    ///
    /// Read-only use only. A turn secret is what a tool holds, and a run that has
    /// ended must still be able to read the results of the children it started --
    /// that is exactly what makes a conversation resumable instead of amnesiac.
    /// So the check does *not* require the turn to still be open; it requires the
    /// secret to be one this daemon minted, whose fence still names the live
    /// binding and this generation. A replacement driver mints a new binding
    /// epoch, which invalidates every older turn secret by construction.
    pub(crate) fn is_issued_session_turn(
        &self,
        root_task_id: &TaskId,
        capability: &str,
    ) -> Result<bool, RuntimeCoordinatorError> {
        let state = self
            .session_delegation_state()
            .lock()
            .expect("session delegation mutex poisoned");
        let Some(live) = state.bindings.get(&root_task_id.to_string()) else {
            return Ok(false);
        };
        if live.fence.generation != self.generation {
            return Ok(false);
        }
        let issued = match state.turns.get(&root_task_id.to_string()) {
            Some(open) => &open.turn,
            None => return Ok(false),
        };
        if issued.fence != live.fence {
            return Ok(false);
        }
        Ok(constant_time_eq(issued.capability.expose(), capability))
    }

    /// Verifies the stop proofs a prepare presents for the old driver's
    /// executions, and reports the ones left unproven.
    ///
    /// A presented reference is an address, never a fact: every one is read back
    /// from the durable evidence table and re-checked against the execution's own
    /// fence. Otherwise a host could retire a live execution by naming a row that
    /// was never written, or one that proves only the join.
    fn settle_replaced_driver(
        &self,
        root: &TaskId,
        request: &PrepareSession,
    ) -> Result<
        (
            Vec<crate::repository::SessionHostExecution>,
            Vec<crate::repository::RetiredHostExecution>,
        ),
        RuntimeCoordinatorError,
    > {
        let repository = self
            .repository
            .lock()
            .expect("runtime repository mutex poisoned");
        let unsettled = repository.unsettled_session_host_executions(root)?;
        if unsettled.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        let mut retired = Vec::new();
        let mut missing = Vec::new();
        for execution in &unsettled {
            // Two places can hold this execution's proof, and both are legitimate:
            //
            // * the foreground evidence table, when the proof came from the
            //   guardian's quiescence flow (the wire names it by `evidence_id`);
            // * the execution row itself, when the host reported the stop on its own
            //   control stream -- `record_host_execution_stop` writes it *there*, and
            //   only there.
            //
            // Reading just the first table made every host-reported stop invisible,
            // so a replaced driver was refused `StopUnproven` forever even though the
            // daemon was holding the proof. Fall back to the row, and re-check every
            // fence field below exactly as strictly: the row is data, not a permit.
            let referenced = request
                .stopped
                .iter()
                .find(|reference| reference.execution_id == execution.execution_id)
                .map(|reference| repository.stop_evidence_by_id(root, &reference.evidence_id))
                .transpose()?
                .flatten();
            let stored = match (referenced, execution.evidence_json.as_deref()) {
                (Some(evidence), _) => Some(evidence),
                (None, Some(json)) => Some(
                    serde_json::from_str::<yi_agent_lifecycle::foreground::StopEvidence>(json)
                        .map_err(|error| RuntimeCoordinatorError::SessionDelegation {
                            code: SessionFailureCode::CorruptAttachment,
                            reason: format!("a stored stop proof is unreadable: {error}"),
                        })?,
                ),
                (None, None) => None,
            };
            let proof = stored;
            let proven = proof.as_ref().is_some_and(|evidence| {
                may_replace_driver(evidence.joined, evidence.tools_stopped)
                    && evidence.execution.execution_id == execution.execution_id
                    && evidence.execution.task_id == execution.root_task_id
                    && evidence.fence.generation == execution.generation
                    && evidence.fence.binding_epoch == execution.binding_epoch
                    && evidence.fence.driver_epoch == execution.driver_epoch
            });
            match (proven, proof) {
                (true, Some(evidence)) => retired.push(crate::repository::RetiredHostExecution {
                    execution_id: execution.execution_id.clone(),
                    evidence_json: serde_json::to_string(&evidence).map_err(|error| {
                        RuntimeCoordinatorError::SessionDelegation {
                            code: SessionFailureCode::CorruptAttachment,
                            reason: format!("a stored stop proof is unreadable: {error}"),
                        }
                    })?,
                    execution_set_version: execution.execution_set_version + 1,
                }),
                _ => missing.push(execution.clone()),
            }
        }
        drop(repository);
        Ok((missing, retired))
    }

    /// Turns on turn enforcement for tests that must prove a bypass is refused.
    ///
    /// `cfg(test)`-style hook: a production daemon can only get here through the
    /// foreground activation, which is what Task 11 flips. Keeping it explicit
    /// means a test cannot pass by accident on a daemon that never enforces.
    #[cfg(any(test, feature = "foreground-test-support"))]
    pub fn enforce_session_turns_for_test(&self) {
        self.session_delegation_enforced
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Checks that a write names the open turn, when the root is a container.
    ///
    /// Returns `Ok(())` for a task that is not a container: execution routes keep
    /// their own task/attempt authority, and this must not become a second gate
    /// they were never designed for. For a container, a missing turn is a refusal
    /// rather than "legacy": there is no legacy write path a container should keep.
    pub fn admit_session_write(
        &self,
        root_task_id: &TaskId,
        turn: Option<&SessionTurn>,
    ) -> Result<(), RuntimeCoordinatorError> {
        // Every other entry point in this module is additive: a host that never
        // prepares keeps working exactly as before. This one is the seam where a
        // write would start being *refused*, so it stays behind the same switch the
        // rest of the lifecycle uses until the hosts that issue turns ship.
        if !self
            .session_delegation_enforced
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Ok(());
        }
        let role = self
            .repository
            .lock()
            .expect("runtime repository mutex poisoned")
            .task_role(root_task_id)
            .or_else(|error| match error {
                crate::repository::RepositoryError::TaskNotFound { .. } => Ok(TaskRole::Execution),
                other => Err(other),
            })?;
        if role != TaskRole::SessionContainer {
            return Ok(());
        }
        let Some(turn) = turn else {
            return Err(RuntimeCoordinatorError::session_delegation(
                SessionFailureCode::SessionIdentityRequired,
                "a session write must name the open turn",
            ));
        };
        self.authorize_session_write(turn)
    }

    fn authenticate_session_identity(
        &self,
        root: &TaskId,
        record: &crate::repository::SessionContainerRecord,
        request: &PrepareSession,
    ) -> Result<(), RuntimeCoordinatorError> {
        match &request.identity {
            SessionIdentity::Managed(authorization) => {
                // Managed identity is authenticated by the guardian ledger, which
                // the attach path already resolved. Nothing here may invent it, and
                // an unmanaged daemon must not accept a managed claim it cannot check.
                let managed = self
                    .managed_authorizations
                    .lock()
                    .expect("managed authorizations mutex poisoned");
                let Some(ledger) = managed.as_ref() else {
                    return Err(RuntimeCoordinatorError::session_delegation(
                        SessionFailureCode::AuthorityDenied,
                        "this daemon has no guardian; a managed identity cannot be checked",
                    ));
                };
                ledger
                    .resolve(&authorization.capability)
                    .and_then(|_| ledger.verify(authorization))
                    .map_err(|reason| {
                        RuntimeCoordinatorError::session_delegation(
                            SessionFailureCode::AuthorityDenied,
                            reason,
                        )
                    })?;
                if authorization.root_id != request.root.root_id
                    || authorization.client_run_id != request.host_run_id
                {
                    return Err(RuntimeCoordinatorError::session_delegation(
                        SessionFailureCode::AuthorityDenied,
                        "the managed authorization names another root or host",
                    ));
                }
                if request.replacement_resume_digest.is_some() {
                    return Err(RuntimeCoordinatorError::session_delegation(
                        SessionFailureCode::AuthorityDenied,
                        "a managed identity is not upgraded by a prepare",
                    ));
                }
                Ok(())
            }
            SessionIdentity::Embedded(secret) => {
                let Some(expected) = record.resume_digest.as_deref() else {
                    return Err(RuntimeCoordinatorError::SessionDelegation {
                        code: SessionFailureCode::SessionIdentityRequired,
                        reason: "this container has no embedded resume digest".to_string(),
                    });
                };
                if !constant_time_eq(expected, &session_resume_digest(secret)) {
                    return Err(RuntimeCoordinatorError::SessionDelegation {
                        code: SessionFailureCode::AuthorityDenied,
                        reason: "the resume secret does not match this container".to_string(),
                    });
                }
                if request.replacement_resume_digest.is_some() {
                    return Err(RuntimeCoordinatorError::SessionDelegation {
                        code: SessionFailureCode::AuthorityDenied,
                        reason: "an embedded identity is not upgraded by a prepare".to_string(),
                    });
                }
                Ok(())
            }
            SessionIdentity::LegacyUpgrade(secret) => {
                let digest = super::digest_hex(secret.expose());
                let attached = self
                    .repository
                    .lock()
                    .expect("runtime repository mutex poisoned")
                    .application_root_capability_matches(
                        &root_session_id(&request.root.session_id)?,
                        root,
                        &digest,
                    )?;
                if !attached {
                    return Err(RuntimeCoordinatorError::SessionDelegation {
                        code: SessionFailureCode::AuthorityDenied,
                        reason: "the legacy application capability is not the attached one"
                            .to_string(),
                    });
                }
                if record.legacy_upgrade_consumed {
                    return Err(RuntimeCoordinatorError::SessionDelegation {
                        code: SessionFailureCode::AuthorityDenied,
                        reason: "this legacy capability was already consumed".to_string(),
                    });
                }
                Ok(())
            }
        }
    }

    fn session_delegation_state(&self) -> &State {
        &self.session_delegation
    }

    fn new_session_secret(&self) -> String {
        format!("{}-{}", self.generation, uuid::Uuid::new_v4())
    }
}

fn root_session_id(value: &str) -> Result<yi_agent_core::RootSessionId, RuntimeCoordinatorError> {
    value.parse().map_err(|_| {
        RuntimeCoordinatorError::session_delegation(
            SessionFailureCode::CorruptAttachment,
            "the session id is not a root session id".to_string(),
        )
    })
}

/// A digest of the request's identity-bearing fields.
///
/// The resume secret is never included: it is authenticated against the stored
/// digest, and folding it in would put a second copy of a secret-derived value in
/// the operations table.
fn request_digest(request: &PrepareSession) -> String {
    let identity = match &request.identity {
        SessionIdentity::Managed(_) => "managed".to_string(),
        SessionIdentity::Embedded(_) => "embedded".to_string(),
        SessionIdentity::LegacyUpgrade(_) => "legacy_upgrade".to_string(),
    };
    let material = [
        request.root.session_id.as_str(),
        request.root.root_id.as_str(),
        request.host_run_id.as_str(),
        &request.expected_binding_epoch.to_string(),
        identity.as_str(),
        request.replacement_resume_digest.as_deref().unwrap_or(""),
    ]
    .join("\u{1f}");
    let digest = Sha256::digest(material.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Compares two digest strings without an early exit on the first difference.
fn constant_time_eq(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.bytes()
        .zip(right.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use yi_agent_lifecycle::foreground::SecretCapability;
    use yi_agent_lifecycle::session_delegation::{SessionFence, SessionRef};

    fn turn(turn_id: &str, secret: &str) -> SessionTurn {
        SessionTurn {
            fence: SessionFence {
                root: SessionRef {
                    session_id: "s".to_string(),
                    root_id: "r".to_string(),
                },
                generation: "g".to_string(),
                binding_epoch: 1,
                driver_epoch: 1,
            },
            turn_id: turn_id.into(),
            capability: SecretCapability::new(secret),
        }
    }

    #[test]
    fn session_delegation_lifecycle_replacement_needs_both_stop_facts() {
        assert!(!may_replace_driver(false, false));
        assert!(!may_replace_driver(true, false));
        assert!(!may_replace_driver(false, true));
        assert!(may_replace_driver(true, true));
    }

    #[test]
    fn session_delegation_lifecycle_old_turn_is_not_refreshed() {
        let a = turn("a", "secret-a");
        let mut b = a.clone();
        b.turn_id = "b".into();
        assert_eq!(
            validate_turn(&b, &a, true),
            Err(SessionFailureCode::StaleTurn)
        );
        assert_eq!(
            validate_turn(&a, &a, false),
            Err(SessionFailureCode::AdmissionClosed)
        );
        assert_eq!(validate_turn(&a, &a, true), Ok(()));
    }

    #[test]
    fn session_delegation_lifecycle_stale_epoch_outranks_a_closed_turn() {
        let a = turn("a", "secret-a");
        let mut moved = a.clone();
        moved.fence.binding_epoch = 2;
        assert_eq!(
            validate_turn(&moved, &a, false),
            Err(SessionFailureCode::StaleBinding),
            "an old generation must be told to re-prepare, not to retry"
        );
    }

    #[test]
    fn session_delegation_lifecycle_a_borrowed_secret_is_not_the_turn() {
        let a = turn("a", "secret-a");
        let forged = turn("a", "secret-b");
        assert_eq!(
            validate_turn(&a, &forged, true),
            Err(SessionFailureCode::AuthorityDenied)
        );
    }

    #[test]
    fn session_delegation_lifecycle_binding_secret_is_not_in_debug() {
        let binding = SessionBinding {
            fence: turn("a", "secret-a").fence,
            capability: SecretCapability::new("binding-secret"),
        };
        let debug = format!("{binding:?}");
        assert!(!debug.contains("binding-secret"), "{debug}");
    }
}
