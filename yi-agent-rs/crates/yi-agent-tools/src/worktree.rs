use std::path::{Path, PathBuf};
use std::process::Command;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum WorktreeError {
    #[error("parent worktree is dirty: {path}")]
    DirtyParent { path: PathBuf },
    #[error("child worktree path reuses its parent: {path}")]
    ParentPathReuse { path: PathBuf },
    #[error("child worktree is dirty: {path}")]
    DirtyChild { path: PathBuf },
    #[error("parent base commit is not available: {base}")]
    UnknownBase { base: String },
    #[error("child branch must merge into recorded parent {expected}, not {actual}")]
    WrongParentBranch { expected: String, actual: String },
    #[error("child branch {branch} has not been merged into {parent}")]
    ChildNotMerged { branch: String, parent: String },
    #[error("git command failed: {message}")]
    Git { message: String },
}

#[derive(Debug, Default)]
pub struct WorktreeService;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildWorktree {
    pub path: PathBuf,
    pub branch: String,
    pub parent_branch: String,
    pub base_commit: String,
}

impl WorktreeService {
    pub fn new() -> Self {
        Self
    }

    /// Reject uncommitted parent state before a child branch can be derived.
    pub fn validate_parent_base(
        &self,
        parent_worktree: &Path,
        base: &str,
    ) -> Result<(), WorktreeError> {
        let status = git(parent_worktree, &["status", "--porcelain"])?;
        if !status.trim().is_empty() {
            return Err(WorktreeError::DirtyParent {
                path: parent_worktree.to_path_buf(),
            });
        }
        let check = Command::new("git")
            .args(["cat-file", "-e", &format!("{base}^{{commit}}")])
            .current_dir(parent_worktree)
            .output()
            .map_err(|error| WorktreeError::Git {
                message: error.to_string(),
            })?;
        if !check.status.success() {
            return Err(WorktreeError::UnknownBase {
                base: base.to_owned(),
            });
        }
        Ok(())
    }

    pub fn create_child(
        &self,
        parent_worktree: &Path,
        base: &str,
        branch: &str,
        child_path: &Path,
    ) -> Result<ChildWorktree, WorktreeError> {
        if same_path(parent_worktree, child_path) {
            return Err(WorktreeError::ParentPathReuse {
                path: child_path.to_path_buf(),
            });
        }
        self.validate_parent_base(parent_worktree, base)?;
        let parent_branch = current_branch(parent_worktree)?;
        let output = Command::new("git")
            .args(["worktree", "add"])
            .arg(child_path)
            .args(["-b", branch, base])
            .current_dir(parent_worktree)
            .output()
            .map_err(|error| WorktreeError::Git {
                message: error.to_string(),
            })?;
        if !output.status.success() {
            return Err(WorktreeError::Git {
                message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        Ok(ChildWorktree {
            path: child_path.to_path_buf(),
            branch: branch.to_owned(),
            parent_branch,
            base_commit: base.to_owned(),
        })
    }

    /// Integrate a clean delivery into exactly the branch that created it.
    pub fn merge_accepted(
        &self,
        parent_worktree: &Path,
        child: &ChildWorktree,
        message: &str,
    ) -> Result<(), WorktreeError> {
        let parent_branch = current_branch(parent_worktree)?;
        if parent_branch != child.parent_branch {
            return Err(WorktreeError::WrongParentBranch {
                expected: child.parent_branch.clone(),
                actual: parent_branch,
            });
        }
        let merge_base = git(
            parent_worktree,
            &["merge-base", &child.branch, &parent_branch],
        )?;
        if merge_base.trim() != child.base_commit {
            return Err(WorktreeError::UnknownBase {
                base: child.base_commit.clone(),
            });
        }
        let output = Command::new("git")
            .args(["merge", "--no-ff", &child.branch, "-m", message])
            .current_dir(parent_worktree)
            .output()
            .map_err(|error| WorktreeError::Git {
                message: error.to_string(),
            })?;
        if !output.status.success() {
            return Err(WorktreeError::Git {
                message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        Ok(())
    }

    /// Remove a delivery only after the recorded parent contains its branch.
    pub fn remove_accepted_clean(
        &self,
        owner_worktree: &Path,
        child: &ChildWorktree,
    ) -> Result<(), WorktreeError> {
        let parent_branch = current_branch(owner_worktree)?;
        if parent_branch != child.parent_branch {
            return Err(WorktreeError::WrongParentBranch {
                expected: child.parent_branch.clone(),
                actual: parent_branch,
            });
        }
        let merged = Command::new("git")
            .args(["merge-base", "--is-ancestor", &child.branch, &parent_branch])
            .current_dir(owner_worktree)
            .status()
            .map_err(|error| WorktreeError::Git {
                message: error.to_string(),
            })?;
        if !merged.success() {
            return Err(WorktreeError::ChildNotMerged {
                branch: child.branch.clone(),
                parent: parent_branch,
            });
        }
        self.remove_clean(owner_worktree, &child.path)
    }

    pub fn remove_clean(
        &self,
        owner_worktree: &Path,
        child_path: &Path,
    ) -> Result<(), WorktreeError> {
        let status = git(child_path, &["status", "--porcelain"])?;
        if !status.trim().is_empty() {
            return Err(WorktreeError::DirtyChild {
                path: child_path.to_path_buf(),
            });
        }
        let output = Command::new("git")
            .args(["worktree", "remove"])
            .arg(child_path)
            .current_dir(owner_worktree)
            .output()
            .map_err(|error| WorktreeError::Git {
                message: error.to_string(),
            })?;
        if !output.status.success() {
            return Err(WorktreeError::Git {
                message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        Ok(())
    }
}

fn same_path(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

fn current_branch(workdir: &Path) -> Result<String, WorktreeError> {
    Ok(git(workdir, &["branch", "--show-current"])?
        .trim()
        .to_owned())
}

fn git(workdir: &Path, args: &[&str]) -> Result<String, WorktreeError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(workdir)
        .output()
        .map_err(|error| WorktreeError::Git {
            message: error.to_string(),
        })?;
    if !output.status.success() {
        return Err(WorktreeError::Git {
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
