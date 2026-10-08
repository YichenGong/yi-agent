use std::path::PathBuf;

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

/// 一张卡片 = 一个需求 = 一对 spec + plan 文件。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CardId(pub String);

impl CardId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

impl std::fmt::Display for CardId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 卡片种类。缺省（含旧状态文件）为实现卡。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CardKind {
    #[default]
    Implementation,
    /// 只做分支合并，不跑会话、不写代码。
    Merge,
}

/// 卡片状态。`Running` 是唯一占用并发槽位的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CardState {
    Queued,
    Launching,
    Running,
    /// 合并卡执行合并时的状态；不占用会话槽位。
    Merging,
    NeedsYou,
    AwaitingMerge,
    Failed,
    Done,
    Paused,
    Cancelled,
}

impl CardState {
    /// 只有 `Running` 占用并发槽位；其余状态一律让出槽位。
    pub fn occupies_slot(self) -> bool {
        matches!(self, CardState::Running)
    }

    /// 终态：不再有任何自动推进。
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            CardState::Done | CardState::Failed | CardState::Cancelled
        )
    }

    /// 状态迁移合法性。不在此表内的迁移一律拒绝。
    pub fn can_transition_to(self, next: CardState) -> bool {
        use CardState::*;
        if self.is_terminal() {
            return false;
        }
        if self == next {
            return false;
        }
        // 取消：任何非终态都可以。
        if next == Cancelled {
            return true;
        }
        match (self, next) {
            (Queued, Running) => true,
            (Queued, Merging) => true,
            (Merging, Done) => true,
            (Merging, NeedsYou) => true,
            (Merging, Failed) => true,
            (Queued, Launching) => true,
            (Launching, Running) => true,
            (Launching, Failed) => true,
            (Launching, Cancelled) => true, // 由上面的 `next == Cancelled` 兜底也行，显式更清楚
            (Running, AwaitingMerge) => true,
            // 合并阶段：实现卡停在 awaiting_merge/needs_you，用户发话后进 merging。
            // 合并轮由卡片原会话里的一轮 turn 执行，故这一步是「人已发话」的记录。
            (AwaitingMerge, Merging) => true,
            (NeedsYou, Merging) => true,
            (Running, NeedsYou) => true,
            (Running, Failed) => true,
            (Running, Paused) => true,
            // 卡片会话被「追问」或自动继续：等待验收的卡重新跑起来。
            // 调度器对账只能看到某一条空闲快照，会话稍后继续时，卡片必须先
            // 从 `AwaitingMerge`/`NeedsYou` 回到 `Running`，否则看板会长期
            // 停留在错误的终态（见 2026-10-03 看板状态误判）。
            (AwaitingMerge, Running) => true,
            (NeedsYou, Running) => true,
            (Paused, Queued) => true,
            (NeedsYou, Queued) => true,
            (AwaitingMerge, Done) => true,
            _ => false,
        }
    }
}

/// 队列中的一张卡片。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Card {
    pub id: CardId,
    pub spec_path: PathBuf,
    pub plan_path: PathBuf,
    pub state: CardState,
    pub enqueued_at: DateTime<Local>,
    /// 排序键：越小越靠前。手动插队会把它压到当前最小值之下（因此可为负）。
    pub order: i64,
    /// 该卡片会话要跑在哪个 worktree。入队时为 `None`；插件在启动前用
    /// `git worktree add` 预建好目录再填入。旧状态文件没有此字段 → 反序列化为 `None`。
    #[serde(default)]
    pub workdir: Option<PathBuf>,
    /// 该卡片启动后 daemon 给它的根任务 id。用它向 daemon 查这张卡片跑完没有，
    /// 完成的卡片让出并发名额。旧状态文件没有此字段 → 反序列化为 `None`。
    #[serde(default)]
    pub task_id: Option<String>,
    /// 卡片会话在 app-server 里的 thread id。启动后回填。
    #[serde(default)]
    pub thread_id: Option<String>,
    /// 启动时 worktree 的 HEAD，供对账判断「有无新提交」。
    #[serde(default)]
    pub base_commit: Option<String>,
    /// 卡片种类；旧状态文件没有此字段 → 实现卡。
    #[serde(default)]
    pub kind: CardKind,
    /// 合并卡的源分支。
    #[serde(default)]
    pub source_ref: Option<String>,
    /// 合并卡的目标分支。
    #[serde(default)]
    pub base_ref: Option<String>,
    /// 自动派生时指向配对的实现卡。
    #[serde(default)]
    pub origin_card: Option<CardId>,
    /// 已归档：隐藏但仍保留在 board.json 里。旧状态文件缺字段 → false。
    #[serde(default)]
    pub archived: bool,
    /// 进入终态（done/failed/cancelled）的时刻；供宽限期自动归档判断。
    #[serde(default)]
    pub terminal_at: Option<DateTime<Local>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_card_without_archive_fields_defaults_to_visible_and_no_terminal_time() {
        let json = r#"{"id":"a","spec_path":"a.spec.md","plan_path":"a.plan.md","state":"queued","enqueued_at":"2026-10-01T00:00:00+08:00","order":0}"#;
        let card: Card = serde_json::from_str(json).unwrap();
        assert!(!card.archived, "旧 board.json 缺字段必须回 false");
        assert_eq!(card.terminal_at, None);
    }

    #[test]
    fn only_running_occupies_a_slot() {
        assert!(CardState::Running.occupies_slot());
        for state in [
            CardState::Queued,
            CardState::NeedsYou,
            CardState::AwaitingMerge,
            CardState::Failed,
            CardState::Done,
            CardState::Paused,
            CardState::Cancelled,
        ] {
            assert!(!state.occupies_slot(), "{state:?} must not occupy a slot");
        }
    }

    #[test]
    fn terminal_states_are_done_failed_cancelled() {
        assert!(CardState::Done.is_terminal());
        assert!(CardState::Failed.is_terminal());
        assert!(CardState::Cancelled.is_terminal());
        assert!(!CardState::AwaitingMerge.is_terminal());
        assert!(!CardState::NeedsYou.is_terminal());
    }

    #[test]
    fn happy_path_transitions_are_allowed() {
        use CardState::*;
        assert!(Queued.can_transition_to(Running));
        assert!(Running.can_transition_to(AwaitingMerge));
        assert!(AwaitingMerge.can_transition_to(Done));
        assert!(Running.can_transition_to(NeedsYou));
        assert!(NeedsYou.can_transition_to(Queued));
        assert!(Running.can_transition_to(Failed));
    }

    #[test]
    fn needs_you_never_holds_a_slot_so_it_can_return_to_the_queue() {
        assert!(!CardState::NeedsYou.occupies_slot());
        assert!(CardState::NeedsYou.can_transition_to(CardState::Queued));
    }

    #[test]
    fn a_reviewed_card_can_resume_running_so_follow_up_turns_stay_visible() {
        use CardState::*;
        // 对账在一条空闲快照上把卡判成 `AwaitingMerge`（或 `NeedsYou`）后，
        // 会话继续（追问/自动续跑）必须能把卡翻回 `Running`，否则看板会显示
        // 错误的终态。
        assert!(AwaitingMerge.can_transition_to(Running));
        assert!(NeedsYou.can_transition_to(Running));
        // 终态仍然不能复活。
        assert!(!Done.can_transition_to(Running));
        assert!(!Failed.can_transition_to(Running));
    }

    #[test]
    fn pause_and_resume_are_allowed_only_around_running() {
        use CardState::*;
        assert!(Running.can_transition_to(Paused));
        assert!(Paused.can_transition_to(Queued));
        assert!(!Paused.can_transition_to(AwaitingMerge));
    }

    #[test]
    fn cancel_is_allowed_from_every_non_terminal_state() {
        use CardState::*;
        for state in [Queued, Running, NeedsYou, AwaitingMerge, Paused] {
            assert!(
                state.can_transition_to(Cancelled),
                "{state:?} must be cancellable"
            );
        }
        assert!(!Done.can_transition_to(Cancelled));
        assert!(!Cancelled.can_transition_to(Queued));
    }

    #[test]
    fn a_running_card_cannot_be_started_twice() {
        assert!(!CardState::Running.can_transition_to(CardState::Running));
    }

    #[test]
    fn a_card_defaults_to_an_implementation_kind() {
        assert_eq!(CardKind::default(), CardKind::Implementation);
    }

    #[test]
    fn a_board_json_without_a_kind_reads_back_as_implementation() {
        // 旧 board.json 没有 kind/source_ref/base_ref/origin_card 字段。
        let json = r#"{"cards":[{"id":"a","spec_path":"a.spec.md","plan_path":"a.plan.md",
            "state":"queued","enqueued_at":"2026-10-01T00:00:00+08:00","order":0}],
            "next_order":1}"#;
        let board: crate::board::Board = serde_json::from_str(json).unwrap();
        let card = board.get(&CardId::new("a")).unwrap();
        assert_eq!(card.kind, CardKind::Implementation);
        assert_eq!(card.source_ref, None);
        assert_eq!(card.origin_card, None);
    }

    #[test]
    fn launching_does_not_occupy_a_slot() {
        assert!(!CardState::Launching.occupies_slot());
    }

    #[test]
    fn queued_can_launch_and_launching_can_run_or_fail() {
        use CardState::*;
        assert!(Queued.can_transition_to(Launching));
        assert!(Launching.can_transition_to(Running));
        assert!(Launching.can_transition_to(Failed));
        assert!(!Launching.can_transition_to(AwaitingMerge));
    }

    #[test]
    fn merging_does_not_occupy_a_provider_slot() {
        assert!(!CardState::Merging.occupies_slot());
    }

    #[test]
    fn a_merging_card_can_finish_or_stop() {
        use CardState::*;
        assert!(Queued.can_transition_to(Merging));
        assert!(Merging.can_transition_to(Done));
        assert!(Merging.can_transition_to(NeedsYou));
        assert!(Merging.can_transition_to(Failed));
        assert!(Merging.can_transition_to(Cancelled));
        // 合并卡绝不能回到会话通路。
        assert!(!Merging.can_transition_to(Running));
        assert!(!Merging.can_transition_to(Launching));
    }

    #[test]
    fn a_card_may_enter_and_leave_the_merge_stage() {
        use CardState::*;
        // 实现卡停在 awaiting_merge，用户发话后进 merging；失败退回 needs_you 再来。
        assert!(AwaitingMerge.can_transition_to(Merging));
        assert!(NeedsYou.can_transition_to(Merging));
        assert!(Merging.can_transition_to(Done));
        assert!(Merging.can_transition_to(NeedsYou));
        // 合并阶段不占会话槽位（口径不变）。
        assert!(!Merging.occupies_slot());
        // 未到 awaiting_merge/needs_you 的卡不得直接进 merging。
        assert!(!Running.can_transition_to(Merging));
        assert!(!Paused.can_transition_to(Merging));
    }
}
