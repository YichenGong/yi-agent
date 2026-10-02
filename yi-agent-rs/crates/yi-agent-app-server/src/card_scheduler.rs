//! 卡片调度器的纯决策逻辑：把「看板卡片 + 本进程跟踪的 thread 状态」映射成动作。
//! 与 I/O 解耦，便于单测；真正的 thread/start、plugin_query 在 server.rs 里执行。

use std::collections::HashMap;

use crate::server::BoardCard;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome { AwaitingMerge, NeedsYou, Failed }

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CardAction {
    Reconcile { card_id: String, thread_id: String, outcome: Outcome },
}

#[derive(Debug, Clone)]
pub(crate) struct TrackedThread {
    pub thread_id: String,
    pub idle: bool,
    pub failed: bool,
    pub needs_you: bool,
}

pub(crate) fn plan(cards: &[BoardCard], tracked: &HashMap<String, TrackedThread>) -> Vec<CardAction> {
    cards.iter().filter_map(|card| {
        if card.state != "running" { return None; }
        let thread_id = card.thread_id.clone()?;
        let t = tracked.get(&card.id)?;
        if !t.idle { return None; }
        let outcome = if t.failed { Outcome::Failed }
            else if t.needs_you { Outcome::NeedsYou }
            else { Outcome::AwaitingMerge };
        Some(CardAction::Reconcile { card_id: card.id.clone(), thread_id, outcome })
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::BoardCard;
    use std::collections::HashMap;

    // A running card always carries a thread_id in production: `board.mark_running`
    // sets it before `board_cards` is read. The helper must supply one for the
    // `running` cases, otherwise `plan` (which needs `card.thread_id`) sees nothing.
    fn card(id: &str, state: &str) -> BoardCard {
        BoardCard { id: id.into(), state: state.into(), thread_id: Some(format!("t-{id}")), workdir: None, spec_path: format!("{id}.spec.md") }
    }

    #[test]
    fn a_queued_card_is_not_launched_by_plan_because_the_plugin_owns_slots() {
        // plan() 只负责「对账已 tracked 的卡片」；启动由 next_launch 驱动，不在 plan 里。
        let actions = plan(&[card("a", "queued")], &Default::default());
        assert!(actions.is_empty());
    }

    #[test]
    fn a_tracked_card_whose_thread_finished_with_changes_awaits_merge() {
        let mut tracked = HashMap::new();
        tracked.insert("a".to_string(), TrackedThread { thread_id: "t1".into(), idle: true, failed: false, needs_you: false });
        let actions = plan(&[card("a", "running")], &tracked);
        assert!(matches!(actions.as_slice(),
            [CardAction::Reconcile { card_id, outcome: Outcome::AwaitingMerge, .. }] if card_id == "a"));
    }

    #[test]
    fn an_idle_thread_without_changes_needs_you() {
        let mut tracked = HashMap::new();
        tracked.insert("a".to_string(), TrackedThread { thread_id: "t1".into(), idle: true, failed: false, needs_you: true });
        let actions = plan(&[card("a", "running")], &tracked);
        assert!(matches!(&actions[0], CardAction::Reconcile { outcome: Outcome::NeedsYou, .. }));
    }
}
