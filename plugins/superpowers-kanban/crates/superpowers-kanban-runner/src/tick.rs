//! 看板推进的纯文案部分。
//!
//! 会话的创建、对账与槽位记账仍不在插件主循环里：决策入口是
//! `BoardService::driver_step`（宿主经 `driver/next` 拉一拍），主循环只消费
//! inbox 与做收敛扫描。这里只剩「给会话的 objective 文案」——它由 `driver_step`
//! 随动作交给宿主，措辞本身是契约。

use std::path::Path;

/// 实现卡**首轮**的 objective：让会话把计划跑完、并把集成选项摆给用户。
///
/// 与宿主旧 `card_scheduler.rs::objective_for` **逐字等价**（宿主不再复刻文案，
/// spec §4.3），所以这条文案是跨进程契约的一部分：`Never merge yourself` 必须
/// 保留——首轮不该合并，合并是一轮**独立**的、明确要求执行的回合
/// （见 [`merge_objective`]）。
pub fn engagement_objective(card: &superpowers_kanban_core::card::Card) -> String {
    engagement_objective_for_paths(
        &card.plan_path.display().to_string(),
        &card.spec_path.display().to_string(),
    )
}

/// [`engagement_objective`] 的路径形态：给「卡片已不在本拍快照里」的退化情形用。
pub fn engagement_objective_for_paths(plan_path: &str, spec_path: &str) -> String {
    format!(
        "Implement the plan at {plan_path} following its spec at {spec_path}. \
         Work only in this worktree. Use superpowers:subagent-driven-development \
         (or superpowers:executing-plans) to execute it, then \
         superpowers:finishing-a-development-branch to present the integration \
         options to the user. Never merge yourself. If you are blocked, report BLOCKED."
    )
}

/// 合并轮的 objective：**另一段**文案，明确要求在这一轮里把 `source` 合进 `base`。
///
/// 与实现轮分开是刻意的（spec §4.6）：同一条「Never merge yourself」若也用在
/// 合并轮上，合并就会被自己禁止。约束只留必要的三条——不 push、不删分支
/// （文末 jest git 动作留在本地，复核以 `merge-base --is-ancestor` 为准），
/// 以及冲突就地解决。
pub fn merge_objective(source: &str, base: &str, workdir: &Path) -> String {
    format!(
        "Merge the branch {source} into {base} here in this worktree ({workdir}). \
         Resolve any conflicts in place and commit the merge. \
         Do not push and do not delete the source branch. \
         When you are done, stop and report the outcome.",
        workdir = workdir.display(),
    )
}

/// 旧名，保留以免动到既有调用方与既有测试。
///
/// 它是实现轮文案的别名：合并轮文案是**另一段**（[`merge_objective`]），
/// 这个函数永不返回它。`driver_step` 自己走 [`engagement_objective`]。
pub fn objective_for(card: &superpowers_kanban_core::card::Card) -> String {
    engagement_objective(card)
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

    /// 实现轮文案与宿主旧 `objective_for` **逐字**一致：它是契约（宿主不再复刻，
    /// spec §4.3），改动会静默改变所有新会话的行为。
    #[test]
    fn the_engagement_objective_is_verbatim_the_old_wording() {
        let board = board_with(&["a"]);
        let card = board.get(&CardId::new("a")).unwrap().clone();
        assert_eq!(
            engagement_objective(&card),
            "Implement the plan at a.plan.md following its spec at a.spec.md. \
             Work only in this worktree. Use superpowers:subagent-driven-development \
             (or superpowers:executing-plans) to execute it, then \
             superpowers:finishing-a-development-branch to present the integration \
             options to the user. Never merge yourself. If you are blocked, report BLOCKED."
        );
    }

    /// 合并轮是**另一段**文案：明确要求执行合并、指名 source/base/workdir，
    /// 且不再带着「别合并」那句自相矛盾的约束。
    #[test]
    fn the_merge_objective_asks_for_the_merge_and_names_source_base_and_workdir() {
        let objective = merge_objective("kanban/card-1", "main", Path::new("/w/card-1"));
        assert!(objective.contains("kanban/card-1"), "{objective}");
        assert!(objective.contains("main"), "{objective}");
        assert!(objective.contains("/w/card-1"), "{objective}");
        assert!(objective.to_lowercase().contains("merge"), "{objective}");
        assert!(
            !objective.contains("Never merge yourself"),
            "合并轮不能被上一段的禁令捆住：{objective}"
        );
        // 本地动作的边界仍要说清楚。
        assert!(objective.contains("Do not push"), "{objective}");
        assert!(
            !objective.contains("delete the source branch. When")
                || objective.contains("do not delete the source branch"),
            "{objective}"
        );
    }

    /// 两段文案必须真的不同——否则「拆开」只是改名。
    #[test]
    fn the_two_objectives_are_distinct_texts() {
        let board = board_with(&["a"]);
        let card = board.get(&CardId::new("a")).unwrap().clone();
        assert_ne!(
            engagement_objective(&card),
            merge_objective("kanban/a", "main", Path::new("/w/a"))
        );
    }
}
