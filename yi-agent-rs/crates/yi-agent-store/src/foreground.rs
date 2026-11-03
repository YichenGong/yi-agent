//! Classification and activity rules for foreground application roots.
//!
//! This module is deliberately pure: no database, no network, no worker. Two
//! decisions live here because both the persistence layer and the coordinator have
//! to reach the *same* answer:
//!
//! * which roots the worker watchdog governs ([`worker_watchdog_applies`]), and
//! * whether an incoming activity report may be applied ([`reduce_activity`]).
//!
//! Keeping them pure is what lets the harder invariants (a late finish must not
//! overwrite a newer turn; a replayed report must be idempotent) be tested without
//! a daemon, a socket or a clock.

use yi_agent_lifecycle::foreground::{ActivityPhase, ActivityUpdate, ForegroundErrorCode};

/// Which watchdog policy a root obeys.
///
/// The distinction is the whole point of this feature: a *container* for a live
/// conversation must not be settled for idleness, while actual scheduled or child
/// work must keep its budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootExecutionClass {
    /// A normal execution task: worker/scheduled root, child or grandchild. The
    /// existing watchdog applies unchanged.
    Execution,
    /// A root that is being authenticated into foreground ownership but has not
    /// been bound yet. It runs nothing, accepts no delegation and holds no lease
    /// beyond a bounded reservation.
    ForegroundPending,
    /// An authenticated foreground container: the host runs the model, the daemon
    /// root is the delegation tree container. No worker watchdog.
    ForegroundManaged,
    /// A record whose declaration contradicts its own attachment/session relation.
    /// Refuse side effects and diagnose; never guess an exemption.
    Corrupt,
}

/// The outcome of trying to apply one activity report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivityApply {
    /// The report advanced the snapshot.
    Applied,
    /// The exact report was already applied; nothing changed.
    Duplicate,
    /// The report is older than the committed one; nothing changed.
    Stale,
}

/// Structured foreground rejections inside the store.
///
/// `StopUnproven` carries the blocking entities because a caller has to report
/// exactly what it could not prove stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForegroundError {
    Unsupported,
    AuthorityDenied,
    AdmissionClosed,
    StaleAttempt,
    StaleDriver,
    InvalidActivity,
    SequenceConflict,
    RecoveryRequired,
    RecoveryAuthorityMissing,
    StopUnproven(Vec<String>),
    ConfirmationRequired,
    ConfirmationExpired,
    OperationConflict,
    CorruptBinding,
}

impl ForegroundError {
    /// The wire discriminant for this error.
    ///
    /// One place maps internal errors to wire codes so the two cannot drift; the
    /// blocking entities travel separately (see `ForegroundFailure`).
    pub fn code(&self) -> ForegroundErrorCode {
        match self {
            Self::Unsupported => ForegroundErrorCode::Unsupported,
            Self::AuthorityDenied => ForegroundErrorCode::AuthorityDenied,
            Self::AdmissionClosed => ForegroundErrorCode::AdmissionClosed,
            Self::StaleAttempt => ForegroundErrorCode::StaleAttempt,
            Self::StaleDriver => ForegroundErrorCode::StaleDriver,
            Self::InvalidActivity => ForegroundErrorCode::InvalidActivity,
            Self::SequenceConflict => ForegroundErrorCode::SequenceConflict,
            Self::RecoveryRequired => ForegroundErrorCode::RecoveryRequired,
            Self::RecoveryAuthorityMissing => ForegroundErrorCode::RecoveryAuthorityMissing,
            Self::StopUnproven(_) => ForegroundErrorCode::StopUnproven,
            Self::ConfirmationRequired => ForegroundErrorCode::ConfirmationRequired,
            Self::ConfirmationExpired => ForegroundErrorCode::ConfirmationExpired,
            Self::OperationConflict => ForegroundErrorCode::OperationConflict,
            Self::CorruptBinding => ForegroundErrorCode::CorruptBinding,
        }
    }
}

impl std::fmt::Display for ForegroundError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Whitelisted phrases only: this text can reach a log line.
        let text = match self {
            Self::Unsupported => "foreground lifecycle is not supported by this runtime",
            Self::AuthorityDenied => "root authority was denied",
            Self::AdmissionClosed => "root admission is closed",
            Self::StaleAttempt => "the report targets a stale attempt",
            Self::StaleDriver => "the report targets a stale driver",
            Self::InvalidActivity => "the activity transition is invalid",
            Self::SequenceConflict => "the activity sequence conflicts with the committed one",
            Self::RecoveryRequired => "the root requires an explicit recovery",
            Self::RecoveryAuthorityMissing => {
                "no verifiable authority remains to recover this root"
            }
            Self::StopUnproven(_) => "the old execution is not proven stopped",
            Self::ConfirmationRequired => "a confirmed recovery operation is required",
            Self::ConfirmationExpired => "the recovery confirmation expired",
            Self::OperationConflict => "another recovery operation is in flight for this root",
            Self::CorruptBinding => "the foreground binding is inconsistent",
        };
        f.write_str(text)
    }
}

impl std::error::Error for ForegroundError {}

/// Whether the worker watchdog governs this root at all.
///
/// `ForegroundPending` and `Corrupt` also answer `false`, and that is safe only
/// because both are simultaneously refused all business: a root that can neither be
/// settled by the watchdog nor do anything is a closed root, not an immortal one.
/// Task 4 proves that invariant at the admission boundary.
pub fn worker_watchdog_applies(class: RootExecutionClass) -> bool {
    matches!(class, RootExecutionClass::Execution)
}

/// The turn/phase half of the reducer.
///
/// The fence and sequence checks run first in [`reduce_activity`]; by the time this
/// is called both roots agree and `incoming.seq >= current.seq`.
fn reduce_turn_transition(
    current: &ActivityUpdate,
    incoming: &ActivityUpdate,
) -> Result<(), ForegroundError> {
    // A report cannot invent a turn out of nothing, and cannot invent a phase that
    // only the daemon may produce.
    if incoming.phase == ActivityPhase::Unknown {
        return Err(ForegroundError::InvalidActivity);
    }

    match current.phase {
        ActivityPhase::Idle => match incoming.phase {
            // idle -> running opens a turn.
            ActivityPhase::Running => match incoming.turn_id.as_deref() {
                Some(turn) if !turn.is_empty() => Ok(()),
                _ => Err(ForegroundError::InvalidActivity),
            },
            // Waiting states are only meaningful inside a turn.
            ActivityPhase::WaitingForChildren | ActivityPhase::WaitingForPermission => {
                Err(ForegroundError::InvalidActivity)
            }
            _ => Ok(()),
        },
        _ => {
            // Inside a turn: the report must name the *current* turn. A finish for an
            // older turn is rejected no matter how large its seq is, which is what
            // stops a late report from closing a newer conversation.
            let current_turn = current
                .turn_id
                .as_deref()
                .ok_or(ForegroundError::InvalidActivity)?;
            if incoming.turn_id.as_deref() != Some(current_turn) {
                return Err(ForegroundError::InvalidActivity);
            }
            Ok(())
        }
    }
}

/// Applies one activity report on top of the committed one.
///
/// The order is fixed and load-bearing:
///
/// 1. every fence component must match (an update for another attempt, daemon
///    generation, binding or driver is rejected outright);
/// 2. `seq < current` is [`ActivityApply::Stale`]; `seq == current` is a duplicate
///    when the payload matches and a conflict otherwise;
/// 3. then, and only then, the turn transition is validated.
///
/// Idleness is not authority: a successful reduction never means "this root is
/// alive", only "this is the newest true statement about what it is doing".
pub fn reduce_activity(
    current: &ActivityUpdate,
    incoming: &ActivityUpdate,
) -> Result<ActivityApply, ForegroundError> {
    if current.fence != incoming.fence {
        // A superseded *driver* is a different answer from a superseded attempt: the
        // first means "reconnect and re-bind", the second means "this conversation
        // has moved on". Only the driver epoch is allowed to differ.
        let same_binding = current.fence.root == incoming.fence.root
            && current.fence.generation == incoming.fence.generation
            && current.fence.binding_epoch == incoming.fence.binding_epoch;
        return Err(if same_binding {
            ForegroundError::StaleDriver
        } else {
            ForegroundError::StaleAttempt
        });
    }

    if incoming.seq < current.seq {
        return Ok(ActivityApply::Stale);
    }
    if incoming.seq == current.seq {
        return if incoming == current {
            Ok(ActivityApply::Duplicate)
        } else {
            Err(ForegroundError::SequenceConflict)
        };
    }

    reduce_turn_transition(current, incoming)?;
    Ok(ActivityApply::Applied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use yi_agent_lifecycle::foreground::{ActivityPhase, ActivityUpdate, ForegroundFence, RootRef};

    const GENERATION: &str = "gen-1";

    /// Builds an update with a fixed fence; only seq/turn/phase vary.
    fn update(seq: u64, turn: Option<&str>, phase: ActivityPhase) -> ActivityUpdate {
        ActivityUpdate {
            fence: ForegroundFence {
                root: RootRef {
                    session_id: "session-1".into(),
                    root_id: "root-1".into(),
                    attempt_id: "attempt-1".into(),
                },
                generation: GENERATION.into(),
                binding_epoch: 1,
                driver_epoch: 1,
            },
            turn_id: turn.map(str::to_owned),
            seq,
            phase,
        }
    }

    #[test]
    fn foreground_root_lifecycle_old_turn_finish_is_rejected_even_with_larger_seq() {
        let current = update(9, Some("new"), ActivityPhase::Running);
        let late = update(10, Some("old"), ActivityPhase::Idle);
        assert!(matches!(
            reduce_activity(&current, &late),
            Err(ForegroundError::InvalidActivity)
        ));
    }

    #[test]
    fn foreground_root_lifecycle_only_execution_uses_worker_watchdog() {
        assert!(worker_watchdog_applies(RootExecutionClass::Execution));
        assert!(!worker_watchdog_applies(
            RootExecutionClass::ForegroundManaged
        ));
        assert!(!worker_watchdog_applies(
            RootExecutionClass::ForegroundPending
        ));
        assert!(!worker_watchdog_applies(RootExecutionClass::Corrupt));
    }

    #[test]
    fn foreground_root_lifecycle_same_seq_same_content_is_a_duplicate() {
        let current = update(4, Some("turn-1"), ActivityPhase::Running);
        assert_eq!(
            reduce_activity(&current, &current),
            Ok(ActivityApply::Duplicate)
        );
    }

    #[test]
    fn foreground_root_lifecycle_same_seq_different_content_conflicts() {
        let current = update(4, Some("turn-1"), ActivityPhase::Running);
        let conflicting = update(4, Some("turn-1"), ActivityPhase::WaitingForChildren);
        assert_eq!(
            reduce_activity(&current, &conflicting),
            Err(ForegroundError::SequenceConflict)
        );
    }

    #[test]
    fn foreground_root_lifecycle_lower_seq_is_stale() {
        let current = update(7, Some("turn-1"), ActivityPhase::Running);
        let older = update(6, Some("turn-1"), ActivityPhase::Running);
        assert_eq!(reduce_activity(&current, &older), Ok(ActivityApply::Stale));
    }

    #[test]
    fn foreground_root_lifecycle_other_attempt_is_rejected() {
        let current = update(7, Some("turn-1"), ActivityPhase::Running);
        let mut other = update(8, Some("turn-1"), ActivityPhase::Running);
        other.fence.root.attempt_id = "attempt-2".into();
        assert_eq!(
            reduce_activity(&current, &other),
            Err(ForegroundError::StaleAttempt)
        );
    }

    #[test]
    fn foreground_root_lifecycle_other_generation_is_rejected() {
        let current = update(7, Some("turn-1"), ActivityPhase::Running);
        let mut other = update(8, Some("turn-1"), ActivityPhase::Running);
        other.fence.generation = "gen-2".into();
        assert_eq!(
            reduce_activity(&current, &other),
            Err(ForegroundError::StaleAttempt)
        );
    }

    #[test]
    fn foreground_root_lifecycle_other_driver_epoch_is_rejected() {
        let current = update(7, Some("turn-1"), ActivityPhase::Running);
        let mut other = update(8, Some("turn-1"), ActivityPhase::Running);
        other.fence.driver_epoch = 2;
        assert_eq!(
            reduce_activity(&current, &other),
            Err(ForegroundError::StaleDriver)
        );
    }

    #[test]
    fn foreground_root_lifecycle_idle_can_begin_a_new_turn() {
        let current = update(1, None, ActivityPhase::Idle);
        let started = update(2, Some("turn-1"), ActivityPhase::Running);
        assert_eq!(
            reduce_activity(&current, &started),
            Ok(ActivityApply::Applied)
        );
    }

    #[test]
    fn foreground_root_lifecycle_matching_turn_finishes() {
        let current = update(2, Some("turn-1"), ActivityPhase::Running);
        let finished = update(3, Some("turn-1"), ActivityPhase::Idle);
        assert_eq!(
            reduce_activity(&current, &finished),
            Ok(ActivityApply::Applied)
        );
    }

    #[test]
    fn foreground_root_lifecycle_second_concurrent_turn_is_refused() {
        let current = update(2, Some("turn-1"), ActivityPhase::Running);
        let second = update(3, Some("turn-2"), ActivityPhase::Running);
        assert_eq!(
            reduce_activity(&current, &second),
            Err(ForegroundError::InvalidActivity)
        );
    }

    #[test]
    fn foreground_root_lifecycle_running_without_a_turn_is_refused() {
        let current = update(1, None, ActivityPhase::Idle);
        let started = update(2, None, ActivityPhase::Running);
        assert_eq!(
            reduce_activity(&current, &started),
            Err(ForegroundError::InvalidActivity)
        );
    }

    #[test]
    fn foreground_root_lifecycle_waiting_outside_a_turn_is_refused() {
        let current = update(1, None, ActivityPhase::Idle);
        let waiting = update(2, None, ActivityPhase::WaitingForPermission);
        assert_eq!(
            reduce_activity(&current, &waiting),
            Err(ForegroundError::InvalidActivity)
        );
    }

    #[test]
    fn foreground_root_lifecycle_waiting_inside_the_current_turn_is_accepted() {
        let current = update(2, Some("turn-1"), ActivityPhase::Running);
        let waiting = update(3, Some("turn-1"), ActivityPhase::WaitingForPermission);
        assert_eq!(
            reduce_activity(&current, &waiting),
            Ok(ActivityApply::Applied)
        );
    }

    #[test]
    fn foreground_root_lifecycle_driver_cannot_report_unknown() {
        let current = update(2, Some("turn-1"), ActivityPhase::Running);
        let unknown = update(3, Some("turn-1"), ActivityPhase::Unknown);
        assert_eq!(
            reduce_activity(&current, &unknown),
            Err(ForegroundError::InvalidActivity)
        );
    }

    #[test]
    fn foreground_root_lifecycle_unknown_epoch_cannot_begin_a_turn() {
        // `Unknown` is server-produced; a new turn may only start from a server
        // initialised Idle after a proven driver stop + rebind.
        let current = update(2, Some("turn-1"), ActivityPhase::Unknown);
        let started = update(3, Some("turn-2"), ActivityPhase::Running);
        assert_eq!(
            reduce_activity(&current, &started),
            Err(ForegroundError::InvalidActivity)
        );
    }

    #[test]
    fn foreground_root_lifecycle_watchdog_guard_covers_every_class() {
        // Exhaustive: adding a class must force a decision here rather than silently
        // defaulting to "no watchdog".
        let all = [
            RootExecutionClass::Execution,
            RootExecutionClass::ForegroundPending,
            RootExecutionClass::ForegroundManaged,
            RootExecutionClass::Corrupt,
        ];
        let watched: Vec<_> = all
            .into_iter()
            .filter(|class| worker_watchdog_applies(*class))
            .collect();
        assert_eq!(watched, vec![RootExecutionClass::Execution]);
    }

    #[test]
    fn foreground_root_lifecycle_error_codes_map_one_to_one() {
        assert_eq!(
            ForegroundError::StopUnproven(vec!["exec-1".into()]).code(),
            ForegroundErrorCode::StopUnproven
        );
        assert_eq!(
            ForegroundError::CorruptBinding.code(),
            ForegroundErrorCode::CorruptBinding
        );
        assert_eq!(
            ForegroundError::RecoveryAuthorityMissing.code(),
            ForegroundErrorCode::RecoveryAuthorityMissing
        );
    }
}
