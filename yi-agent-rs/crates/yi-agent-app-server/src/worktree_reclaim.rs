//! 卡片会话 worktree 的判定与删除。
//!
//! 判定只吃 git 事实：`decide` 读「是否看板 worktree / 是否干净 / 分支状态」，
//! 分类抽成纯函数 `classify` 以便脱 git 单测。只服务卡片会话（`card_id` 非空）；
//! 普通会话的 cwd 是用户项目目录，任何情况都不删。

use std::path::{Path, PathBuf};
use std::process::Command;

/// `kanban/<slug>` 的 slug：与插件 `worktree::slugify` 逐字一致。
///
/// 只保留 ASCII 字母数字（小写化），其余折叠为单个 `-`，首尾 `-` 去掉；
/// 全空时退化为 `card`。不一致会让宿主演化出与插件不同的分支名，
/// 从而把"已并入"误判为"分支缺失"（安全侧失败，但会漏删）。
pub fn slugify(card_id: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in card_id.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "card".to_string()
    } else {
        trimmed
    }
}

/// 项目默认分支：`origin/HEAD` 退化到 `main`（与插件 `merge::default_branch` 同口径）。
pub fn default_branch(project: &Path) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(project)
        .args(["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
        .output();
    if let Ok(out) = out {
        if out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if let Some(name) = text.strip_prefix("origin/") {
                if !name.is_empty() {
                    return name.to_string();
                }
            }
        }
    }
    "main".to_string()
}

/// 源分支相对默认分支的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchState {
    /// `kanban/<slug>` 不存在：无法判定是否已并入。
    Missing,
    /// 存在且已并入默认分支。
    Merged,
    /// 存在但未并入。
    Unmerged,
}

/// 一次 worktree 回收的判定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeReclaim {
    /// 不适用（普通会话）：不做任何 worktree 动作，响应也不带该字段。
    None,
    /// 可无损删除（干净且已并入）：无需确认，直接删。
    Delete { path: PathBuf },
    /// 确认后会永久丢弃内容：需两步确认。
    DestructiveDelete { path: PathBuf, reason: String },
    /// 即便 force 也不删（硬拒绝），需让用户知道工作被保留及原因。
    Keep { path: PathBuf, reason: String },
}

/// 纯分类：只吃布尔事实与分支状态，便于脱 git 单测。
///
/// 顺序即优先级：非卡片会话 → 先排除；不是看板 worktree → 硬保留；
/// 干净且已并入 → 可删；其余（脏/未并入/分支缺失）→ 破坏性。
pub fn classify(
    is_card_session: bool,
    under_kanban_dir: bool,
    in_worktree_list: bool,
    dirty: bool,
    branch: BranchState,
    path: PathBuf,
) -> WorktreeReclaim {
    if !is_card_session {
        return WorktreeReclaim::None;
    }
    if !under_kanban_dir || !in_worktree_list {
        return WorktreeReclaim::Keep {
            path,
            reason: "cwd 不是本项目登记在册的看板 worktree，拒绝删除".to_string(),
        };
    }
    if !dirty && branch == BranchState::Merged {
        return WorktreeReclaim::Delete { path };
    }
    let reason = if dirty {
        "worktree 有未提交改动，确认后将永久丢弃".to_string()
    } else if branch == BranchState::Missing {
        "源分支已不存在，无法确认已并入，确认后将永久丢弃".to_string()
    } else {
        "源分支未并入默认分支，确认后将永久丢弃".to_string()
    };
    WorktreeReclaim::DestructiveDelete { path, reason }
}

/// git 探针：判定一张卡片会话的 worktree 该删、该确认、还是该留。
pub fn decide(card_id: Option<&str>, board_project: Option<&str>, cwd: &Path) -> WorktreeReclaim {
    let Some(card_id) = card_id.filter(|id| !id.is_empty()) else {
        return WorktreeReclaim::None;
    };
    let Some(project) = board_project.filter(|p| !p.is_empty()) else {
        // 卡片会话必然有项目根；缺失时无从判定，安全侧保留。
        return WorktreeReclaim::Keep {
            path: cwd.to_path_buf(),
            reason: "会话未记录所属项目根，拒绝删除".to_string(),
        };
    };
    let project_path = Path::new(project);
    let cwd_canon = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let kanban_dir = project_path.join(".worktrees").join("kanban");
    let kanban_canon = kanban_dir
        .canonicalize()
        .unwrap_or_else(|_| kanban_dir.clone());
    let under = cwd_canon.starts_with(&kanban_canon);
    let in_list = worktree_list_contains(project_path, &cwd_canon);
    let dirty = !git_ok(cwd, &["status", "--porcelain"])
        .unwrap_or_default()
        .trim()
        .is_empty();
    let branch = format!("kanban/{}", slugify(card_id));
    let base = default_branch(project_path);
    let branch_state = if !git_ok(
        project_path,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .is_some()
    {
        BranchState::Missing
    } else if git_status_ok(
        project_path,
        &["merge-base", "--is-ancestor", &branch, &base],
    ) {
        BranchState::Merged
    } else {
        BranchState::Unmerged
    };
    classify(true, under, in_list, dirty, branch_state, cwd_canon)
}

/// 删除一块 worktree：`git -C <project> worktree remove [--force] <path>`。
pub fn remove(path: &Path, project: &Path, force: bool) -> Result<(), String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(project).args(["worktree", "remove"]);
    if force {
        cmd.arg("--force");
    }
    cmd.arg(path);
    let out = cmd
        .output()
        .map_err(|error| format!("could not run git: {error}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// `git -C <dir> <args>` 成功时返回 stdout（失败/未安装 → None）。
fn git_ok(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

/// 只看退出码（不关心输出）。
fn git_status_ok(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// `cwd` 是否出现在 `<project>` 的 `git worktree list --porcelain` 里。
fn worktree_list_contains(project: &Path, cwd: &Path) -> bool {
    let Some(text) = git_ok(project, &["worktree", "list", "--porcelain"]) else {
        return false;
    };
    let target = cwd.to_string_lossy();
    text.lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .any(|listed| {
            let listed_path = Path::new(listed.trim());
            listed_path
                .canonicalize()
                .unwrap_or_else(|_| listed_path.to_path_buf())
                == Path::new(target.as_ref())
        })
}

#[cfg(test)]
mod tests {
    // 测试里用简名 `Reclaim` 指代 `WorktreeReclaim`，与 spec 措辞一致。
    use super::WorktreeReclaim as Reclaim;
    use super::*;
    use std::process::Command;

    /// 建一个真 git 仓库（main 分支，一次提交），返回其 canonical 路径。
    fn init_repo(dir: &Path) -> PathBuf {
        let run = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(dir)
                    .args(args)
                    .status()
                    .unwrap()
                    .success(),
                "git {args:?}"
            );
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        // 与 Task 2 的 `git_project` 同一理由：真实项目的 `.yi-agent/` 是 gitignored 的，
        // 复刻它，判定才是按生产事实走（见 Task 2 里的详细说明）。
        std::fs::write(dir.join(".gitignore"), ".yi-agent/\n.worktrees/\n").unwrap();
        std::fs::write(dir.join("f.txt"), "hi").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "init"]);
        dir.canonicalize().unwrap()
    }

    /// 在 `<project>/.worktrees/kanban/<slug>` 建一块真 worktree，分支 `kanban/<slug>`。
    fn add_kanban_worktree(project: &Path, card_id: &str) -> PathBuf {
        let slug = slugify(card_id);
        let path = project.join(".worktrees/kanban").join(&slug);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let branch = format!("kanban/{slug}");
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(project)
                .args(["worktree", "add", "-q"])
                .arg(&path)
                .args(["-b", &branch])
                .status()
                .unwrap()
                .success()
        );
        path
    }

    fn git(project: &Path, args: &[&str]) {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(project)
                .args(args)
                .status()
                .unwrap()
                .success(),
            "git {args:?}"
        );
    }

    #[test]
    fn slugify_matches_the_plugin_rule() {
        // 必须与插件 `worktree::slugify` 逐字一致，否则"分支名"对不上。
        assert_eq!(slugify("card-1"), "card-1");
        assert_eq!(slugify("Card 2 foo/bar"), "card-2-foo-bar");
        assert_eq!(slugify("///"), "card");
    }

    /// `classify` 的最后一个参数是路径，分类本身不看它——给个占位即可。
    fn wt() -> PathBuf {
        PathBuf::from("/tmp/wt")
    }

    #[test]
    fn classify_prefers_no_reclaim_for_a_plain_session() {
        assert_eq!(
            classify(false, true, true, false, BranchState::Merged, wt()),
            Reclaim::None,
            "普通会话永不回收 worktree"
        );
    }

    #[test]
    fn classify_keeps_when_not_a_kanban_worktree() {
        // 在 worktree list 里但不是看板 worktree 路径 → 硬保留。
        assert!(matches!(
            classify(true, false, true, false, BranchState::Merged, wt()),
            Reclaim::Keep { .. }
        ));
        // 在 kanban 目录下但 git 不认它是 worktree → 也硬保留。
        assert!(matches!(
            classify(true, true, false, false, BranchState::Merged, wt()),
            Reclaim::Keep { .. }
        ));
    }

    #[test]
    fn classify_deletes_silently_when_clean_and_merged() {
        assert!(matches!(
            classify(true, true, true, false, BranchState::Merged, wt()),
            Reclaim::Delete { .. }
        ));
    }

    #[test]
    fn classify_marks_dirty_or_unmerged_or_missing_as_destructive() {
        assert!(matches!(
            classify(true, true, true, true, BranchState::Merged, wt()),
            Reclaim::DestructiveDelete { .. }
        ));
        assert!(matches!(
            classify(true, true, true, false, BranchState::Unmerged, wt()),
            Reclaim::DestructiveDelete { .. }
        ));
        assert!(matches!(
            classify(true, true, true, false, BranchState::Missing, wt()),
            Reclaim::DestructiveDelete { .. }
        ));
    }

    /// 端到端（真 git）：干净且已并入 → 静默可删。
    #[test]
    fn decide_deletes_a_clean_merged_card_worktree() {
        let dir = tempfile::TempDir::new().unwrap();
        let project = init_repo(&dir.path().join("proj").tap_mkdir());
        let wt = add_kanban_worktree(&project, "card-1");
        let p = project.to_string_lossy().to_string();
        let got = decide(Some("card-1"), Some(&p), &wt);
        assert!(matches!(got, Reclaim::Delete { .. }), "{got:?}");
    }

    /// 端到端：脏 worktree → 破坏性（需确认）。
    #[test]
    fn decide_flags_a_dirty_card_worktree() {
        let dir = tempfile::TempDir::new().unwrap();
        let project = init_repo(&dir.path().join("proj").tap_mkdir());
        let wt = add_kanban_worktree(&project, "card-1");
        std::fs::write(wt.join("scratch.txt"), "wip").unwrap();
        let p = project.to_string_lossy().to_string();
        let got = decide(Some("card-1"), Some(&p), &wt);
        match got {
            Reclaim::DestructiveDelete { reason, .. } => {
                assert!(reason.contains("未提交"), "{reason}")
            }
            other => panic!("expected DestructiveDelete, got {other:?}"),
        }
    }

    /// 端到端：分支未并入 → 破坏性。
    #[test]
    fn decide_flags_an_unmerged_branch() {
        let dir = tempfile::TempDir::new().unwrap();
        let project = init_repo(&dir.path().join("proj").tap_mkdir());
        let wt = add_kanban_worktree(&project, "card-1");
        // 分支上多一个未并入 main 的提交。
        std::fs::write(wt.join("more.txt"), "x").unwrap();
        git(&wt, &["add", "-A"]);
        git(&wt, &["commit", "-q", "-m", "wip"]);
        let p = project.to_string_lossy().to_string();
        match decide(Some("card-1"), Some(&p), &wt) {
            Reclaim::DestructiveDelete { reason, .. } => {
                assert!(reason.contains("未并入"), "{reason}")
            }
            other => panic!("expected DestructiveDelete, got {other:?}"),
        }
    }

    /// 端到端：普通会话（card_id=None）→ None。
    #[test]
    fn decide_returns_none_for_a_plain_session() {
        let dir = tempfile::TempDir::new().unwrap();
        let project = init_repo(&dir.path().join("proj").tap_mkdir());
        assert_eq!(
            decide(None, Some(&project.to_string_lossy()), &project),
            Reclaim::None
        );
    }

    /// `remove`：真删一块 worktree；再删一次失败（已不存在）。
    #[test]
    fn remove_deletes_a_linked_worktree() {
        let dir = tempfile::TempDir::new().unwrap();
        let project = init_repo(&dir.path().join("proj").tap_mkdir());
        let wt = add_kanban_worktree(&project, "card-1");
        remove(&wt, &project, false).expect("clean removal succeeds");
        assert!(!wt.exists(), "worktree dir is gone");
        assert!(
            remove(&wt, &project, false).is_err(),
            "second removal fails"
        );
    }

    /// 小工具：把路径当目录建出来，便于链式表达式里构造 temp 子目录。
    trait TapMkdir {
        fn tap_mkdir(&self) -> PathBuf;
    }
    impl TapMkdir for Path {
        fn tap_mkdir(&self) -> PathBuf {
            std::fs::create_dir_all(self).unwrap();
            self.to_path_buf()
        }
    }
}
