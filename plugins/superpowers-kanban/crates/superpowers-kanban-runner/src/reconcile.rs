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
///
/// 先筛候选集、空则**不碰 git**（连 `default_branch` 也不调）：收敛扫描每个 tick
/// 都跑，而候选集通常为空（无 `awaiting_merge` 实现卡）。早退回 `Vec::new()`
/// 才能兑现「无候选卡时零 git 调用」的开销承诺（spec §9）。
pub fn plan<G: GitFacts>(board: &Board, git: &G) -> Vec<Reconcile> {
    let ids = converge::candidates(board);
    if ids.is_empty() {
        return Vec::new();
    }
    let base = git.default_branch();
    let mut reports = Vec::new();
    for id in ids {
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

    /// 探针：任何 git 事实被问及即 panic。用来把「空候选集零 git 调用」钉死——
    /// 若 `plan` 又在候选循环之前先取 `default_branch`，本测试立刻炸。
    struct ForbiddenGit;

    impl GitFacts for ForbiddenGit {
        fn default_branch(&self) -> String {
            panic!("no candidates -> default_branch must not be called")
        }
        fn branch_exists(&self, _source: &str) -> bool {
            panic!("no candidates -> branch_exists must not be called")
        }
        fn branch_merged(&self, _source: &str, _base: &str) -> bool {
            panic!("no candidates -> branch_merged must not be called")
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

    /// 候选集为空时必须零 git 调用（spec §9 的开销承诺）：收敛扫描每 tick 都跑，
    /// 而绝大多数 tick 没有 `awaiting_merge` 实现卡。空板 + 爆炸探针 = 早退生效。
    #[test]
    fn an_empty_candidate_set_never_touches_git() {
        let board = Board::new();
        assert!(plan(&board, &ForbiddenGit).is_empty());
    }

    /// 只有非候选（此处置一张 `needs_you` 卡）时同样不碰 git。
    #[test]
    fn a_board_without_candidates_never_touches_git() {
        let mut board = board_with_awaiting("card-1");
        board
            .transition(&CardId::new("card-1"), CardState::Running)
            .unwrap();
        board
            .transition(&CardId::new("card-1"), CardState::NeedsYou)
            .unwrap();
        assert!(plan(&board, &ForbiddenGit).is_empty());
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

    /// 无 workdir 的缺分支卡（例如迁移来的旧卡）：文案退化到 `unknown`，
    /// 不能让提示行少掉 worktree 段。
    #[test]
    fn a_missing_branch_without_a_workdir_says_unknown() {
        let line = report_line(&Reconcile::MissingBranch {
            id: CardId::new("card-1"),
            source: "kanban/card-1".into(),
            workdir: None,
        });
        assert!(line.contains("unknown"), "{line}");
    }
}
