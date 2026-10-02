//! 看板推进的纯文案部分。
//!
//! 会话的创建、对账与槽位记账已移出插件：插件不再调用
//! `CreateAutonomousSession`，改由宿主经 `board.next_launch` /
//! `board.mark_running` / `board.mark_terminal` / `board.release` 驱动
//! （宿主侧调度器见 plan 的 Task 5/6）。这里只剩「给会话的首轮 objective
//! 文案」——它由 Task 6 起的宿主会话首轮复用，必须保留。

/// The objective handed to the daemon. Kept in one place so the wording (and
/// the Superpowers constraints it carries) is reviewable at a glance.
pub fn objective_for(card: &superpowers_kanban_core::card::Card) -> String {
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
    use superpowers_kanban_core::board::Board;
    use superpowers_kanban_core::card::CardId;

    fn board_with(ids: &[&str]) -> Board {
        let mut board = Board::new();
        for (index, id) in ids.iter().enumerate() {
            board.enqueue(
                CardId::new(*id),
                format!("{id}.spec.md").into(),
                format!("{id}.plan.md").into(),
                Local
                    .with_ymd_and_hms(2026, 10, 1, index as u32, 0, 0)
                    .single()
                    .unwrap(),
            );
        }
        board
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
}
