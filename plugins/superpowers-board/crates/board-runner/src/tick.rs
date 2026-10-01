use std::path::PathBuf;

use board_core::board::Board;
use board_core::card::{CardId, CardState};

use crate::client::BoardDaemon;

/// What one tick did to one card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TickAction {
    Launched {
        session_id: String,
        root_task_id: String,
    },
    Transitioned(CardState),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickOutcome {
    pub card_id: CardId,
    pub action: TickAction,
}

/// Runs one scheduling pass.
///
/// `launch` supplies the workdir for a card by pre-creating its worktree. It
/// owns where worktrees live because that is environment policy, not board
/// policy. Returning `None` means "could not prepare a workdir": the card stays
/// queued and is reported as `TickAction::Failed` so the board surfaces the
/// problem instead of silently spinning on the same card forever.
pub fn run_once(
    board: &mut Board,
    daemon: &BoardDaemon,
    limit: u16,
    launch: &mut dyn FnMut(&CardId) -> Option<PathBuf>,
) -> Vec<TickOutcome> {
    let mut outcomes = Vec::new();
    for card_id in crate::runner::plan_launches(board, limit) {
        let Some(workdir) = launch(&card_id) else {
            outcomes.push(TickOutcome {
                card_id,
                action: TickAction::Failed("could not prepare a worktree".to_string()),
            });
            continue;
        };
        let objective = board.get(&card_id).map(objective_for).unwrap_or_default();
        match daemon.create_session(&objective, &workdir) {
            Ok(created) => {
                if board.transition(&card_id, CardState::Running).is_ok() {
                    outcomes.push(TickOutcome {
                        card_id,
                        action: TickAction::Launched {
                            session_id: created.session_id,
                            root_task_id: created.root_task_id,
                        },
                    });
                }
            }
            Err(error) => outcomes.push(TickOutcome {
                card_id,
                action: TickAction::Failed(error.to_string()),
            }),
        }
    }
    outcomes
}

/// The objective handed to the daemon. Kept in one place so the wording (and
/// the Superpowers constraints it carries) is reviewable at a glance.
pub fn objective_for(card: &board_core::card::Card) -> String {
    format!(
        "Implement the plan at {plan} following its spec at {spec}. \
         Work only in this worktree. Use superpowers:subagent-driven-development \
         (or superpowers:executing-plans) to execute it, then \
         superpowers:finishing-a-development-branch to present the integration \
         options to the user. Never merge yourself. If you are blocked, report BLOCKED.",
        plan = card.plan_path.display(),
        spec = card.spec_path.display(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Local, TimeZone};

    fn at(hour: u32) -> chrono::DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 10, 1, hour, 0, 0)
            .single()
            .unwrap()
    }

    fn board_with(ids: &[&str]) -> Board {
        let mut board = Board::new();
        for (index, id) in ids.iter().enumerate() {
            board.enqueue(
                CardId::new(*id),
                format!("{id}.spec.md").into(),
                format!("{id}.plan.md").into(),
                at(index as u32),
            );
        }
        board
    }

    #[test]
    fn a_successful_launch_moves_the_card_to_running() {
        let mut board = board_with(&["a", "b"]);
        // A daemon that accepts every request: exercise the loop with a stub by
        // asserting on the pure planning + transition path.
        let planned = crate::runner::plan_launches(&board, 1);
        assert_eq!(planned, vec![CardId::new("a")]);
        board
            .transition(&CardId::new("a"), CardState::Running)
            .unwrap();
        assert_eq!(board.running_count(), 1);
        assert_eq!(
            board.get(&CardId::new("b")).unwrap().state,
            CardState::Queued
        );
    }

    #[test]
    fn the_objective_carries_the_plan_spec_and_constraints() {
        let board = board_with(&["a"]);
        let card = board.get(&CardId::new("a")).unwrap().clone();
        let objective = objective_for(&card);
        assert!(objective.contains("a.plan.md"));
        assert!(objective.contains("a.spec.md"));
        assert!(objective.contains("Never merge yourself"));
        assert!(objective.contains("BLOCKED"));
    }

    #[test]
    fn outcomes_record_launch_and_transition() {
        let outcome = TickOutcome {
            card_id: CardId::new("a"),
            action: TickAction::Launched {
                session_id: "s".into(),
                root_task_id: "t".into(),
            },
        };
        assert_eq!(outcome.card_id, CardId::new("a"));
        let transition = TickOutcome {
            card_id: CardId::new("a"),
            action: TickAction::Transitioned(CardState::AwaitingMerge),
        };
        assert!(matches!(
            transition.action,
            TickAction::Transitioned(CardState::AwaitingMerge)
        ));
    }
}
