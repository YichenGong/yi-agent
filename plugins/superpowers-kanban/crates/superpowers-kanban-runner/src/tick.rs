//! 看板推进的纯文案部分。
//!
//! 会话的创建、对账与槽位记账仍不在插件主循环里：决策入口是
//! `BoardService::driver_step`（宿主经 `driver/next` 拉一拍），主循环只消费
//! inbox 与做收敛扫描。这里只剩「给会话的 objective 文案」——它由 `driver_step`
//! 随动作交给宿主，措辞本身是契约。

use std::path::Path;

/// 卡片会话**首轮**的 objective：把计划跑完、把集成选项摆给用户，并接管卡片状态。
///
/// 文案是跨进程契约（宿主不复刻，spec §4.3），两处措辞不能删：
/// `Never merge yourself`（合并要单独走名额，见 [`merge_objective`]）与
/// 「你负责这张卡的状态」。后者是本次改造的核心——看板不再猜，模型不写状态
/// 卡片就停在原处。
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
         options to the user. Never merge yourself. If you are blocked, report BLOCKED.\n\n\
         You own this card's kanban state. The board never guesses it. Before you \
         end a turn, when you need a decision from the user, and when the work is \
         finished, say where the card stands: run \
         `superpowers-kanban state --self <state>` (the plugin injects \
         SUPERPOWERS_KANBAN_CARD into your shell). Use `awaiting_merge` when the \
         plan is done, `needs_you` when you are waiting on the user, and `done` \
         only once it is really merged."
    )
}

/// 把一个 shell 参数安全地包起来（单引号 + 内部单引号转义）。
///
/// 合并指令里会带上分支名、两个目录和一个绝对 state-dir——它们都来自看板数据或
/// 用户输入，不能直接拼进命令。用 `sh -c` 的往返测试锁住这个行为。
pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// 实现卡续跑回合的 objective：**自包含**的一轮合并指令（spec §9）。
///
/// "自包含"是这次需求的硬要求。热会话（原会话还活着）可能仍然记得开发阶段的
/// 范围限制——"只在开发树里工作"、"Never merge yourself"。所以这段话必须：
///
/// - 明确说**本轮授权替代**那条旧的范围限制（但不替代任何权限/sandbox 规则）；
/// - 给出卡片 id、源分支、base、开发树、目标树、**绝对** state-dir，
///   不依赖 `state --self` 的环境变量（冷装配才注入 env，热会话没有）；
/// - 说清 daemon 已经持着名额，不要再 `merge-request`；
/// - 要求合并与验证**同步等待**结束，不留还能写目标树的后台进程；
/// - 不 push、不删源分支、不删开发树（删了以后就无法冷恢复原会话）。
pub fn queued_merge_objective(
    card_id: &str,
    source: &str,
    base: &str,
    development_workdir: &Path,
    target_workdir: &Path,
    state_dir: &Path,
) -> String {
    let card = shell_quote(card_id);
    let source = shell_quote(source);
    let base = shell_quote(base);
    let development = shell_quote(&development_workdir.display().to_string());
    let target = shell_quote(&target_workdir.display().to_string());
    let state = shell_quote(&state_dir.display().to_string());
    format!(
        "This card ({card}) was authorized for merge by the user. This turn **replaces** the \
         earlier scope restriction that told you to work only in the development worktree and \
         never to merge; that restriction is now lifted for this card. It does **not** lift any \
         permission or sandbox rule: keep using the same approval flow as before, and if the \
         target tree cannot be authorized, stop and report needs_you instead of widening \
         access.\n\n\
         The merge: take {source} into {base} in the target worktree {target}. The development \
         worktree {development} is where this conversation started; it is only used to locate \
         this session. Do not merge there. The project merge slot is already held for you by \
         the daemon, so do **not** call `superpowers-kanban merge-request`.\n\n\
         Run the merge and the project's verification **synchronously**: wait for every command \
         and every writer to finish before you declare the outcome. A background process that \
         can still write the target tree is not a finished merge, and \"the session looks idle\" \
         is not proof that some other process stopped. Do not push, do not delete the source \
         branch, and do not delete the development worktree (deleting it would make this \
         conversation impossible to restore later). Resolve conflicts in place and commit \
         them.\n\n\
         When you are done, record the outcome with your own card id and state directory — do \
         not rely on `--self`, because a restored session may not have that environment:\n\
         `superpowers-kanban state {card} done --state-dir {state}` when \
         `git merge-base --is-ancestor {source} {base}` succeeds, otherwise \
         `superpowers-kanban state {card} needs_you --detail '<reason>' --state-dir {state}`.\n\
         Do not call `merge-finish`: the daemon releases the slot on its own once it sees your \
         result and observes this session idle."
    )
}

/// 独立合并卡的 objective：**另一段**文案。
///
/// 与实现轮分开是刻意的（spec §4.6）：同一条「Never merge yourself」若也用在
/// 合并轮上，合并就会被自己禁止。但它同样**不**授权裸 `git merge`——名额是
/// 项目级锁（`merge.lock`），同一时刻只允许一个进程合并，所以流程仍是
/// `merge-request` → 合并 → `merge-finish`，与用户在会话里发话时一致。
pub fn merge_objective(source: &str, base: &str, workdir: &Path) -> String {
    format!(
        "This card exists to merge the branch {source} into {base}. \
         Do not run git merge directly: first take the project merge slot with \
         `superpowers-kanban merge-request <card-id>`, then merge in the worktree it \
         returns ({workdir} is this session's tree, which may not be the right one). \
         Resolve any conflicts in place and commit the merge. \
         Do not push and do not delete the source branch. \
         Then release the slot with `superpowers-kanban merge-finish <card-id>` and \
         record the outcome with `superpowers-kanban state --self <state>`: `done` when \
         `git merge-base --is-ancestor {source} {base}` succeeds, otherwise \
         `needs_you` with the reason.",
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

    /// 实现轮文案是契约（宿主不复刻，spec §4.3）：改动会静默改变所有新会话的
    /// 行为，所以这里逐字钉住。旧约束（`Never merge yourself` / `BLOCKED`）必须
    /// 还在——新加的职责说明不能把它们挤掉。
    #[test]
    fn the_engagement_objective_is_verbatim() {
        let board = board_with(&["a"]);
        let card = board.get(&CardId::new("a")).unwrap().clone();
        assert_eq!(
            engagement_objective(&card),
            "Implement the plan at a.plan.md following its spec at a.spec.md. \
             Work only in this worktree. Use superpowers:subagent-driven-development \
             (or superpowers:executing-plans) to execute it, then \
             superpowers:finishing-a-development-branch to present the integration \
             options to the user. Never merge yourself. If you are blocked, report BLOCKED.\n\n\
             You own this card's kanban state. The board never guesses it. Before you \
             end a turn, when you need a decision from the user, and when the work is \
             finished, say where the card stands: run \
             `superpowers-kanban state --self <state>` (the plugin injects \
             SUPERPOWERS_KANBAN_CARD into your shell). Use `awaiting_merge` when the \
             plan is done, `needs_you` when you are waiting on the user, and `done` \
             only once it is really merged."
        );
    }

    /// 模型必须知道「状态归你管」这件事，且知道**怎么**管（动词名 + 环境变量）。
    /// 只写「请维护状态」而不给命令，等于没写。
    #[test]
    fn the_objective_tells_the_session_it_owns_the_card_state() {
        let board = board_with(&["a"]);
        let card = board.get(&CardId::new("a")).unwrap().clone();
        let objective = engagement_objective(&card);
        assert!(
            objective.contains("superpowers-kanban state"),
            "{objective}"
        );
        assert!(objective.contains("--self"), "{objective}");
        assert!(objective.contains("SUPERPOWERS_KANBAN_CARD"), "{objective}");
        assert!(objective.contains("Never merge yourself"), "{objective}");
        assert!(objective.contains("BLOCKED"), "{objective}");
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
        // 但也不能授权裸 merge：名额是项目级锁，绕过去就会两条分支同改主检出。
        assert!(objective.contains("merge-request"), "{objective}");
        assert!(objective.contains("merge-finish"), "{objective}");
        assert!(
            objective.contains("Do not run git merge directly"),
            "{objective}"
        );
        // 本地动作的边界仍要说清楚。
        assert!(objective.contains("Do not push"), "{objective}");
        assert!(
            !objective.contains("delete the source branch. When")
                || objective.contains("do not delete the source branch"),
            "{objective}"
        );
    }

    fn flat(text: &str) -> String {
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// 续跑指令必须自包含:明确 card id、两个目录、绝对 state-dir,且要求
    /// 同步等待——不能留还能写目标树的后台进程（spec §9）。
    #[test]
    fn merge_authorization_is_explicit_and_location_safe() {
        let text = queued_merge_objective(
            "card-1",
            "kanban/card-1",
            "main",
            Path::new("/w/dev"),
            Path::new("/w/base tree"),
            Path::new("/p/.yi-agent/kanban"),
        );
        assert!(text.contains("card-1"));
        assert!(text.contains("--state-dir '/p/.yi-agent/kanban'"));
        assert!(text.contains("'/w/base tree'"));
        assert!(text.contains("'/w/dev'"));
        assert!(!text.contains("<card-id>"));
        assert!(!text.contains("state --self"));
        assert!(!text.contains("release the slot with"));
        let flat = flat(&text);
        assert!(flat.contains("merge slot is already held"), "{flat}");
        assert!(
            flat.contains("do **not** call `superpowers-kanban merge-request`"),
            "{flat}"
        );
        // 同步等待是硬要求:后台进程能写目标树就等于没合完。
        assert!(flat.contains("**synchronously**"), "{flat}");
        // 本轮授权替代旧的范围限制,但不替代权限/sandbox。
        assert!(
            flat.contains("replaces the earlier scope restriction")
                || flat.contains("**replaces** the earlier scope restriction"),
            "{flat}"
        );
        assert!(
            flat.contains("does **not** lift any permission or sandbox rule"),
            "{flat}"
        );
        assert!(flat.contains("Do not push"), "{flat}");
        assert!(
            flat.contains("do not delete the development worktree"),
            "{flat}"
        );
    }

    /// 单引号、空格、`$` 都必须原样到达 shell,不能被解释或截断。
    #[test]
    fn shell_quoting_survives_a_real_shell_round_trip() {
        for value in [
            "plain",
            "with space",
            "quote's here",
            "$HOME and $(id)",
            "semi;colon and other-command",
            "back slash",
        ] {
            let quoted = shell_quote(value);
            let output = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("printf %s {quoted}"))
                .output()
                .expect("sh runs");
            assert!(output.status.success(), "sh failed for {value:?}");
            assert_eq!(
                String::from_utf8_lossy(&output.stdout),
                value,
                "quoting must round-trip {value:?}"
            );
        }
    }

    /// 一条含注入内容的参数不能真的执行里面的命令。
    #[test]
    fn shell_quoting_does_not_execute_injected_commands() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("marker");
        let attack = format!("x; touch {}", marker.display());
        let quoted = shell_quote(&attack);
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("printf %s {quoted}"))
            .output()
            .expect("sh runs");
        assert_eq!(String::from_utf8_lossy(&output.stdout), attack);
        assert!(
            !marker.exists(),
            "an injected command must stay literal text"
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
