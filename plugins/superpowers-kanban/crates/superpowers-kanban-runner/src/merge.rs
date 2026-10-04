//! 合并执行引擎：把 source 合进 base。
//!
//! **在 base 被检出的 worktree 里执行**（若 base 正是项目主检出的分支，就在主检出里合），
//! 因此主检出不会因为「base 没检出」而被切换/污染——这正是 CLAUDE.md 里
//! 「回 main 做 git merge --no-ff」的人工约定。base 未被任何 worktree 检出时，
//! 才另建一个专用 worktree，用完移除。

use std::path::{Path, PathBuf};
use std::process::Command;

use superpowers_kanban_core::card_id::slug;

#[derive(Debug, PartialEq, Eq)]
pub enum MergeOutcome {
    Merged,
    Conflict,
    GitError(String),
}

pub struct BaseWorktree {
    pub path: PathBuf,
    /// 这个 worktree 是本引擎新建的（用完可移除）；主检出复用时为 false。
    pub created: bool,
}

pub fn default_branch(project_root: &Path) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(project_root)
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

/// `source` 是否已并入 `base`（即 base 是 source 的祖先，或两者相等）。
pub fn branch_merged_into(project_root: &Path, source: &str, base: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["merge-base", "--is-ancestor", source, base])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

pub fn source_branch_exists(project_root: &Path, source: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{source}"),
        ])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// base 已被检出的 worktree 路径（通常是主检出）。
fn worktree_for_branch(project_root: &Path, base: &str) -> Option<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut current: Option<PathBuf> = None;
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            current = Some(PathBuf::from(path));
        } else if line == format!("branch refs/heads/{base}") {
            return current;
        }
    }
    None
}

/// 定位（或创建）执行合并的 worktree。创建失败视为配置问题（base 名写错等），返错误。
pub fn prepare(project_root: &Path, base: &str) -> Result<BaseWorktree, String> {
    if let Some(path) = worktree_for_branch(project_root, base) {
        return Ok(BaseWorktree {
            path,
            created: false,
        });
    }
    let path = project_root
        .join(".worktrees")
        .join("kanban-merge")
        .join(slug(base));
    if path.join(".git").exists() {
        return Ok(BaseWorktree {
            path,
            created: true,
        });
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["worktree", "add"])
        .arg(&path)
        .arg(base)
        .output()
        .map_err(|e| format!("could not run git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "could not check out base '{base}': {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(BaseWorktree {
        path,
        created: true,
    })
}

pub fn is_dirty(wt: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(wt)
        .args(["status", "--porcelain"])
        .output()
        .map(|out| !String::from_utf8_lossy(&out.stdout).trim().is_empty())
        .unwrap_or(true)
}

pub fn run(wt: &Path, source: &str, base: &str) -> MergeOutcome {
    let message = format!("merge {source} into {base} (kanban)");
    let out = Command::new("git")
        .arg("-C")
        .arg(wt)
        .args(["merge", "--no-ff", source, "-m", &message])
        .output();
    let out = match out {
        Ok(out) => out,
        Err(error) => return MergeOutcome::GitError(format!("could not run git: {error}")),
    };
    if out.status.success() {
        return MergeOutcome::Merged;
    }
    // 失败：有未合并路径 = 冲突；一律先 abort 复原，保持 base worktree 干净可重试。
    let unmerged = Command::new("git")
        .arg("-C")
        .arg(wt)
        .args(["diff", "--name-only", "--diff-filter=U"])
        .output()
        .map(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty())
        .unwrap_or(false);
    let _ = Command::new("git")
        .arg("-C")
        .arg(wt)
        .args(["merge", "--abort"])
        .output();
    if unmerged {
        MergeOutcome::Conflict
    } else {
        MergeOutcome::GitError(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// 只移除本引擎创建的 worktree；复用主检出时绝不动它。
pub fn cleanup(wt: &BaseWorktree) {
    if !wt.created {
        return;
    }
    let _ = Command::new("git")
        .arg("-C")
        .arg(&wt.path)
        .args(["worktree", "remove", "--force"])
        .arg(&wt.path)
        .output();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(dir: &Path) {
        let git = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(dir)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "b@t"]);
        git(&["config", "user.name", "B"]);
        std::fs::write(dir.join("f.txt"), "base\n").unwrap();
        git(&["add", "f.txt"]);
        git(&["commit", "-qm", "seed"]);
    }

    #[test]
    fn a_clean_merge_moves_the_base_and_leaves_it_clean() {
        let dir = tempfile::tempdir().unwrap();
        repo(dir.path());
        let git = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(dir.path())
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        git(&["checkout", "-qb", "feat/x"]);
        std::fs::write(dir.path().join("x.txt"), "x\n").unwrap();
        git(&["add", "x.txt"]);
        git(&["commit", "-qm", "feat"]);
        git(&["checkout", "-q", "main"]);
        assert!(source_branch_exists(dir.path(), "feat/x"));

        let base = prepare(dir.path(), "main").unwrap();
        assert_eq!(run(&base.path, "feat/x", "main"), MergeOutcome::Merged);
        assert!(
            dir.path().join("x.txt").exists(),
            "base worktree got the file"
        );
        assert!(
            !is_dirty(&base.path),
            "the base worktree is clean after merging"
        );
        cleanup(&base);
    }

    #[test]
    fn a_conflict_is_aborted_and_reported() {
        let dir = tempfile::tempdir().unwrap();
        repo(dir.path());
        let git = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(dir.path())
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        git(&["checkout", "-qb", "feat/c"]);
        std::fs::write(dir.path().join("f.txt"), "one\n").unwrap();
        git(&["commit", "-qam", "c"]);
        git(&["checkout", "-q", "main"]);
        std::fs::write(dir.path().join("f.txt"), "two\n").unwrap();
        git(&["commit", "-qam", "m"]);

        let base = prepare(dir.path(), "main").unwrap();
        assert_eq!(run(&base.path, "feat/c", "main"), MergeOutcome::Conflict);
        assert!(
            !is_dirty(&base.path),
            "conflict must be aborted, not left half-merged"
        );
        cleanup(&base);
    }

    #[test]
    fn a_missing_source_branch_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        repo(dir.path());
        assert!(!source_branch_exists(dir.path(), "kanban/nope"));
    }
}
