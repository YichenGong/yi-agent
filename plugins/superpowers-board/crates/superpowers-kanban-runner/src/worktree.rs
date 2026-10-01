use std::path::{Path, PathBuf};
use std::process::Command;

use superpowers_kanban_core::card::CardId;

#[derive(Debug)]
pub enum WorktreeError {
    /// `git worktree add` 退出码非零。
    GitFailed(String),
    /// 无法启动 git（未安装等）。
    Spawn(std::io::Error),
}

impl std::fmt::Display for WorktreeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorktreeError::GitFailed(message) => write!(f, "git worktree add failed: {message}"),
            WorktreeError::Spawn(error) => write!(f, "could not run git: {error}"),
        }
    }
}

impl std::error::Error for WorktreeError {}

/// 分支名与目录名都要求是文件系统友好的：只保留 ASCII 字母数字和 `-`，
/// 其余字符折叠为 `-`。卡号本身通常是 `card-1` 这类，slug 主要防意外字符。
pub fn slugify(id: &CardId) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in id.0.chars() {
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

/// 插件预建的隔离 worktree 落点：`<project>/.worktrees/kanban/<slug>`。
pub fn worktree_path(project_root: &Path, id: &CardId) -> PathBuf {
    project_root
        .join(".worktrees")
        .join("kanban")
        .join(slugify(id))
}

/// 若该 worktree 已存在则直接返回（幂等：重启后不重复建）；否则
/// `git worktree add <path> -b <branch>`。**纯本地操作，不消耗模型调用。**
pub fn ensure_worktree(
    project_root: &Path,
    id: &CardId,
    branch: &str,
) -> Result<PathBuf, WorktreeError> {
    let path = worktree_path(project_root, id);
    if path.join(".git").exists() {
        return Ok(path);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(WorktreeError::Spawn)?;
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .arg("worktree")
        .arg("add")
        .arg(&path)
        .arg("-b")
        .arg(branch)
        .output()
        .map_err(WorktreeError::Spawn)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(WorktreeError::GitFailed(stderr.trim().to_string()));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_keeps_letters_digits_and_collapses_the_rest() {
        assert_eq!(slugify(&CardId::new("card-1")), "card-1");
        assert_eq!(slugify(&CardId::new("Card 2 foo/bar")), "card-2-foo-bar");
        assert_eq!(
            slugify(&CardId::new("///")),
            "card",
            "never collapses to empty"
        );
    }

    #[test]
    fn the_worktree_lives_under_the_project_root() {
        assert_eq!(
            worktree_path(Path::new("/proj"), &CardId::new("card-1")),
            PathBuf::from("/proj/.worktrees/kanban/card-1")
        );
    }

    #[test]
    fn ensure_worktree_creates_a_real_git_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let git = |args: &[&str]| {
            let status = Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "board@example.test"]);
        git(&["config", "user.name", "Board Test"]);
        std::fs::write(root.join("README.md"), "seed\n").unwrap();
        git(&["add", "README.md"]);
        git(&["commit", "-q", "-m", "seed"]);

        let id = CardId::new("card-1");
        let path = ensure_worktree(root, &id, "kanban/card-1-demo").unwrap();
        assert!(path.join(".git").exists(), "a real worktree was created");

        // 幂等：第二次调用不再建，直接返回同一路径。
        let again = ensure_worktree(root, &id, "kanban/card-1-demo").unwrap();
        assert_eq!(again, path);
    }
}
