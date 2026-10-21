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

/// 卡片状态。
///
/// 这里**不再**记录「谁在占并发槽位」——并发按宿主上报的活会话计（见
/// `superpowers_kanban_runner::service`），与卡片状态解耦。一张卡被标了
/// `Done` 但会话还开着，它仍然占名额，因为它确实还在烧 token。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CardState {
    /// 已决定要做，但 spec/plan 还没写出来；不进会话。
    PreQueued,
    Queued,
    Running,
    NeedsYou,
    AwaitingMerge,
    Failed,
    Done,
    Cancelled,
}

impl CardState {
    /// 终态：不再有任何自动推进。
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            CardState::Done | CardState::Failed | CardState::Cancelled
        )
    }

    /// 状态迁移合法性。
    ///
    /// 写状态的是**模型**（`superpowers-kanban state`），它有上下文；整张迁移表
    /// 只会在正当场景上挡住它（历史上就有「卡还在 queued 但活儿已经做完」）。
    /// 唯一保留的规则是**终态不可复活**：那是一条承诺，复活它等于让看板撒谎。
    pub fn can_transition_to(self, next: CardState) -> bool {
        !self.is_terminal() && self != next
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
    /// 提出这张卡的**讨论会话**。与 `thread_id`（跑这张卡的执行会话）语义不同，
    /// 故不复用后者：`thread_id` 会被调度器的 `mark_running` 覆写成执行会话，
    /// 复用它会让溯源线在提升/开跑时断掉。旧 board.json 缺此字段 → `None`。
    #[serde(default)]
    pub origin_thread_id: Option<String>,
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
    /// 这张卡**为什么**落到当前状态：启动失败时就是宿主回报的 `error`
    /// （spec §4.6「失败 → 卡 `failed`（原因取 `error`）」）。
    ///
    /// 落盘并经 `list` 暴露，人才答得出「这张卡为什么失败」——只写日志的话，
    /// 重启一次就没有任何地方能查。旧状态文件没有此字段 → `None`。
    #[serde(default)]
    pub detail: Option<String>,
    /// 展示标题。预排队卡在创建时定下；正式卡为 `None`（标题由 spec_path 派生）。
    #[serde(default)]
    pub title: Option<String>,
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
        assert_eq!(card.detail, None, "旧 board.json 缺 detail 必须回 None");
    }

    #[test]
    fn a_detail_survives_a_serialization_round_trip() {
        // 判词要落盘：daemon 重启后 `list` 仍答得出「这张卡为什么失败」。
        let json = r#"{"id":"a","spec_path":"a.spec.md","plan_path":"a.plan.md","state":"failed","enqueued_at":"2026-10-01T00:00:00+08:00","order":0,"detail":"worktree not found"}"#;
        let card: Card = serde_json::from_str(json).unwrap();
        assert_eq!(card.detail.as_deref(), Some("worktree not found"));
        let back: Card = serde_json::from_str(&serde_json::to_string(&card).unwrap()).unwrap();
        assert_eq!(back.detail.as_deref(), Some("worktree not found"));
    }

    #[test]
    fn any_non_terminal_state_may_move_to_any_other() {
        use CardState::*;
        let live = [PreQueued, Queued, Running, NeedsYou, AwaitingMerge];
        for from in live {
            for to in live {
                if from == to {
                    assert!(
                        !from.can_transition_to(to),
                        "{from:?} -> {to:?} must be rejected"
                    );
                } else {
                    assert!(
                        from.can_transition_to(to),
                        "{from:?} -> {to:?} must be allowed"
                    );
                }
            }
            assert!(from.can_transition_to(Done), "{from:?} -> Done");
            assert!(from.can_transition_to(Failed), "{from:?} -> Failed");
            assert!(from.can_transition_to(Cancelled), "{from:?} -> Cancelled");
        }
    }

    /// 终态是给用户的承诺：复活一张 done 的卡等于让看板撒谎。
    #[test]
    fn a_terminal_state_never_revives() {
        use CardState::*;
        for from in [Done, Failed, Cancelled] {
            for to in [
                PreQueued,
                Queued,
                Running,
                NeedsYou,
                AwaitingMerge,
                Done,
                Failed,
                Cancelled,
            ] {
                assert!(
                    !from.can_transition_to(to),
                    "{from:?} -> {to:?} must be rejected"
                );
            }
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
    fn a_card_cannot_move_to_its_own_state() {
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
    fn a_pre_queued_card_can_be_promoted() {
        use CardState::*;
        assert!(PreQueued.can_transition_to(Queued));
    }

    #[test]
    fn an_old_card_without_a_title_still_deserializes() {
        let json = r#"{"id":"a","spec_path":"a.spec.md","plan_path":"a.plan.md","state":"queued","enqueued_at":"2026-10-01T00:00:00+08:00","order":0}"#;
        let card: Card = serde_json::from_str(json).unwrap();
        assert_eq!(card.title, None, "旧 board.json 缺 title 必须回 None");
    }

    #[test]
    fn an_old_card_without_an_origin_thread_still_deserializes() {
        // 旧 board.json 没有 origin_thread_id：必须回 None，不得整份解析失败。
        let json = r#"{"id":"a","spec_path":"a.spec.md","plan_path":"a.plan.md","state":"queued","enqueued_at":"2026-10-01T00:00:00+08:00","order":0}"#;
        let card: Card = serde_json::from_str(json).unwrap();
        assert_eq!(card.origin_thread_id, None);
    }
}
