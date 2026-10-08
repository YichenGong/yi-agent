//! 收敛扫描：把候选卡按 git 事实分类，产出「该冒出水面」的报告。
//!
//! git 访问经 `GitFacts` 抽象，故分类逻辑可用假探针单测；生产用 `RealGit`。
//! 本模块**不**改看板状态、**不**打印——状态迁移与去重在 `BoardService`，
//! 打印在 tick 循环。

use std::path::{Path, PathBuf};

use superpowers_kanban_core::board::Board;
use superpowers_kanban_core::card::CardId;
use superpowers_kanban_core::converge::{self, Converge};

use crate::merge;
use crate::worktree::slugify;

/// 卡片推导出的源分支：与 `command_done` / `merge_request` 逐字同规则。
pub fn source_for(id: &CardId) -> String {
    format!("kanban/{}", slugify(id))
}

/// git 事实的探针。抽象出来是为了让分类逻辑可脱离真实仓库单测。
pub trait GitFacts {
    fn default_branch(&self) -> String;
    fn branch_exists(&self, source: &str) -> bool;
    fn branch_merged(&self, source: &str, base: &str) -> bool;
}

/// 真实 git 实现；全部委托既有的 `merge.rs` 助手，不另造 git 调用。
pub struct RealGit<'a> {
    pub project_root: &'a Path,
}

impl GitFacts for RealGit<'_> {
    fn default_branch(&self) -> String {
        merge::default_branch(self.project_root)
    }
    fn branch_exists(&self, source: &str) -> bool {
        merge::source_branch_exists(self.project_root, source)
    }
    fn branch_merged(&self, source: &str, base: &str) -> bool {
        merge::branch_merged_into(self.project_root, source, base)
    }
}

/// 一条该冒出水面的收敛结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reconcile {
    /// 分支已并入 base：卡将落 `done`。
    Converged {
        id: CardId,
        source: String,
        base: String,
    },
    /// 分支缺失且本 daemon 尚未提示过：等人工 `done`。
    MissingBranch {
        id: CardId,
        source: String,
        workdir: Option<PathBuf>,
    },
}

/// 扫描候选卡，返回该报告/该落地的结论。**不去重**（去重归 `BoardService`）。
///
/// `Converge::Wait` 不产出任何报告。分支不存在时不调用 `branch_merged`
/// （省一次 git 调用，且语义上无意义）。
pub fn plan<G: GitFacts>(board: &Board, git: &G) -> Vec<Reconcile> {
    let base = git.default_branch();
    let mut reports = Vec::new();
    for id in converge::candidates(board) {
        let source = source_for(&id);
        let existing = git.branch_exists(&source);
        let merged = existing && git.branch_merged(&source, &base);
        match converge::classify(existing, merged) {
            Converge::Done => reports.push(Reconcile::Converged {
                id,
                source,
                base: base.clone(),
            }),
            Converge::Wait => {}
            Converge::MissingBranch => {
                let workdir = board.get(&id).and_then(|card| card.workdir.clone());
                reports.push(Reconcile::MissingBranch {
                    id,
                    source,
                    workdir,
                });
            }
        }
    }
    reports
}

/// 报告行文案（不含 `superpowers-kanban: ` 前缀，由打印侧统一加）。
pub fn report_line(r: &Reconcile) -> String {
    match r {
        Reconcile::Converged { id, source, base } => {
            format!("card {id} auto-converged to done ({source} is merged into {base})")
        }
        Reconcile::MissingBranch {
            id,
            source,
            workdir,
        } => {
            let workdir = workdir
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "unknown".to_string());
            format!(
                "card {id} is awaiting_merge but {source} is gone; \
                 run `superpowers-kanban done {id}` to confirm (worktree: {workdir})"
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Local, TimeZone};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use superpowers_kanban_core::board::Board;
    use superpowers_kanban_core::card::{CardId, CardState};

    fn at() -> chrono::DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 10, 1, 0, 0, 0)
            .single()
            .unwrap()
    }

    /// 一张停在 `awaiting_merge` 的实现卡，带一个 workdir（供缺分支提示）。
    fn board_with_awaiting(id: &str) -> Board {
        let mut board = Board::new();
        let card = CardId::new(id);
        board.enqueue(
            card.clone(),
            format!("{id}.spec.md").into(),
            format!("{id}.plan.md").into(),
            at(),
        );
        board
            .set_workdir(&card, PathBuf::from(format!("/w/{id}")))
            .unwrap();
        board.transition(&card, CardState::Running).unwrap();
        board.transition(&card, CardState::AwaitingMerge).unwrap();
        board
    }

    /// 假探针：`facts[source] = (exists, merged)`。分类逻辑因此无需真实仓库。
    struct FakeGit {
        base: String,
        facts: HashMap<String, (bool, bool)>,
    }

    impl GitFacts for FakeGit {
        fn default_branch(&self) -> String {
            self.base.clone()
        }
        fn branch_exists(&self, source: &str) -> bool {
            self.facts.get(source).map(|(e, _)| *e).unwrap_or(false)
        }
        fn branch_merged(&self, source: &str, _base: &str) -> bool {
            self.facts.get(source).map(|(_, m)| *m).unwrap_or(false)
        }
    }

    fn fake(source: &str, exists: bool, merged: bool) -> FakeGit {
        let mut facts = HashMap::new();
        facts.insert(source.to_string(), (exists, merged));
        FakeGit {
            base: "main".to_string(),
            facts,
        }
    }

    #[test]
    fn a_merged_branch_produces_a_converged_report() {
        let board = board_with_awaiting("card-1");
        let git = fake("kanban/card-1", true, true);
        let reports = plan(&board, &git);
        assert_eq!(
            reports,
            vec![Reconcile::Converged {
                id: CardId::new("card-1"),
                source: "kanban/card-1".into(),
                base: "main".into(),
            }]
        );
    }

    #[test]
    fn an_existing_unmerged_branch_produces_no_report() {
        let board = board_with_awaiting("card-1");
        let git = fake("kanban/card-1", true, false);
        assert!(plan(&board, &git).is_empty());
    }

    #[test]
    fn a_missing_branch_is_reported_with_its_workdir() {
        // plan 自身不去重：同一输入两次都产出报告，去重是 BoardService 的职责。
        let board = board_with_awaiting("card-1");
        let git = fake("kanban/card-1", false, false);
        let expected = vec![Reconcile::MissingBranch {
            id: CardId::new("card-1"),
            source: "kanban/card-1".into(),
            workdir: Some(PathBuf::from("/w/card-1")),
        }];
        assert_eq!(plan(&board, &git), expected);
        assert_eq!(plan(&board, &git), expected, "plan 本身无记忆");
    }

    #[test]
    fn the_source_is_derived_from_the_slugified_card_id() {
        assert_eq!(
            source_for(&CardId::new("Card 2 foo/bar")),
            "kanban/card-2-foo-bar"
        );
    }

    #[test]
    fn report_lines_are_actionable() {
        let converged = report_line(&Reconcile::Converged {
            id: CardId::new("card-1"),
            source: "kanban/card-1".into(),
            base: "main".into(),
        });
        assert!(converged.contains("auto-converged to done"), "{converged}");
        assert!(converged.contains("kanban/card-1"), "{converged}");

        let missing = report_line(&Reconcile::MissingBranch {
            id: CardId::new("card-1"),
            source: "kanban/card-1".into(),
            workdir: Some(PathBuf::from("/w/card-1")),
        });
        assert!(
            missing.contains("superpowers-kanban done card-1"),
            "{missing}"
        );
        assert!(missing.contains("/w/card-1"), "workdir 要带上：{missing}");
    }
}
