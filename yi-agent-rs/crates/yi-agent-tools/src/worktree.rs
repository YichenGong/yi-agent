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
    #[error("child delivery has no commits beyond its recorded base {base}")]
    EmptyDelivery { base: String },
    #[error("reviewed child HEAD {reviewed} changed to {current}")]
    ReviewedHeadChanged { reviewed: String, current: String },
    #[error("parent base commit is not available: {base}")]
    UnknownBase { base: String },
    #[error("recorded base {base} is not the current parent HEAD {head}")]
    BaseIsNotParentHead { base: String, head: String },
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectedDelivery {
    pub branch: String,
    pub base_commit: String,
    pub head_commit: String,
    pub clean: bool,
}

impl WorktreeService {
    pub fn new() -> Self {
        Self
    }

    pub fn create_root(
        &self,
        repository_root: &Path,
        branch: &str,
        root_path: &Path,
    ) -> Result<ChildWorktree, WorktreeError> {
        if same_path(repository_root, root_path) {
            return Err(WorktreeError::ParentPathReuse {
                path: root_path.to_path_buf(),
            });
        }
        self.validate_parent_base(repository_root, "HEAD")?;
        let parent_branch = current_branch(repository_root)?;
        let base_commit = self.resolve_parent_base(repository_root, "HEAD")?;
        ensure_worktree_parent_is_ignored(repository_root, root_path)?;
        add_worktree(repository_root, root_path, branch, &base_commit)?;
        Ok(ChildWorktree {
            path: root_path.to_path_buf(),
            branch: branch.to_owned(),
            parent_branch,
            base_commit,
        })
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
        self.resolve_parent_base(parent_worktree, base)?;
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
        let base_commit = self.resolve_parent_base(parent_worktree, base)?;
        let parent_branch = current_branch(parent_worktree)?;
        ensure_worktree_parent_is_ignored(parent_worktree, child_path)?;
        add_worktree(parent_worktree, child_path, branch, &base_commit)?;
        Ok(ChildWorktree {
            path: child_path.to_path_buf(),
            branch: branch.to_owned(),
            parent_branch,
            base_commit,
        })
    }

    /// Capture the exact clean commit that a parent may review and integrate.
    pub fn inspect_delivery(
        &self,
        child: &ChildWorktree,
    ) -> Result<InspectedDelivery, WorktreeError> {
        let status = git(&child.path, &["status", "--porcelain"])?;
        if !status.trim().is_empty() {
            return Err(WorktreeError::DirtyChild {
                path: child.path.clone(),
            });
        }
        let branch = current_branch(&child.path)?;
        if branch != child.branch {
            return Err(WorktreeError::WrongParentBranch {
                expected: child.branch.clone(),
                actual: branch,
            });
        }
        let head_commit = git(&child.path, &["rev-parse", "HEAD"])?.trim().to_owned();
        let merge_base = git(
            &child.path,
            &["merge-base", &child.base_commit, &head_commit],
        )?;
        if merge_base.trim() != child.base_commit {
            return Err(WorktreeError::UnknownBase {
                base: child.base_commit.clone(),
            });
        }
        if head_commit == child.base_commit {
            return Err(WorktreeError::EmptyDelivery {
                base: child.base_commit.clone(),
            });
        }
        Ok(InspectedDelivery {
            branch: child.branch.clone(),
            base_commit: child.base_commit.clone(),
            head_commit,
            clean: true,
        })
    }

    /// Integrate the reviewed delivery into exactly the branch that created it.
    pub fn merge_accepted(
        &self,
        parent_worktree: &Path,
        child: &ChildWorktree,
        delivery: &InspectedDelivery,
        message: &str,
    ) -> Result<(), WorktreeError> {
        self.merge_inspected_delivery(parent_worktree, child, delivery, message)
    }

    /// Merge only a previously inspected commit, never the moving child branch tip.
    pub fn merge_inspected_delivery(
        &self,
        parent_worktree: &Path,
        child: &ChildWorktree,
        delivery: &InspectedDelivery,
        message: &str,
    ) -> Result<(), WorktreeError> {
        if !delivery.clean {
            return Err(WorktreeError::DirtyChild {
                path: child.path.clone(),
            });
        }
        let parent_branch = current_branch(parent_worktree)?;
        if parent_branch != child.parent_branch {
            return Err(WorktreeError::WrongParentBranch {
                expected: child.parent_branch.clone(),
                actual: parent_branch,
            });
        }
        if delivery.branch != child.branch || delivery.base_commit != child.base_commit {
            return Err(WorktreeError::UnknownBase {
                base: delivery.base_commit.clone(),
            });
        }
        let status = git(&child.path, &["status", "--porcelain"])?;
        if !status.trim().is_empty() {
            return Err(WorktreeError::DirtyChild {
                path: child.path.clone(),
            });
        }
        let child_branch = current_branch(&child.path)?;
        if child_branch != child.branch {
            return Err(WorktreeError::WrongParentBranch {
                expected: child.branch.clone(),
                actual: child_branch,
            });
        }
        let child_head = git(&child.path, &["rev-parse", "HEAD"])?.trim().to_owned();
        if child_head != delivery.head_commit {
            return Err(WorktreeError::ReviewedHeadChanged {
                reviewed: delivery.head_commit.clone(),
                current: child_head,
            });
        }
        if delivery.head_commit == child.base_commit {
            return Err(WorktreeError::EmptyDelivery {
                base: child.base_commit.clone(),
            });
        }
        let merge_base = git(
            parent_worktree,
            &["merge-base", &delivery.head_commit, &parent_branch],
        )?;
        if merge_base.trim() != child.base_commit {
            return Err(WorktreeError::UnknownBase {
                base: child.base_commit.clone(),
            });
        }
        let output = Command::new("git")
            .args(["merge", "--no-ff", &delivery.head_commit, "-m", message])
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
        self.remove_clean(owner_worktree, &child.path)?;
        let output = Command::new("git")
            .args(["branch", "-d", &child.branch])
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

    fn resolve_parent_base(
        &self,
        parent_worktree: &Path,
        base: &str,
    ) -> Result<String, WorktreeError> {
        let resolved_base = git(
            parent_worktree,
            &["rev-parse", "--verify", &format!("{base}^{{commit}}")],
        )
        .map_err(|_| WorktreeError::UnknownBase {
            base: base.to_owned(),
        })?
        .trim()
        .to_owned();
        let parent_head = git(parent_worktree, &["rev-parse", "HEAD"])?
            .trim()
            .to_owned();
        if resolved_base != parent_head {
            return Err(WorktreeError::BaseIsNotParentHead {
                base: resolved_base,
                head: parent_head,
            });
        }
        Ok(parent_head)
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

fn add_worktree(
    owner_worktree: &Path,
    path: &Path,
    branch: &str,
    base_commit: &str,
) -> Result<(), WorktreeError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| WorktreeError::Git {
            message: error.to_string(),
        })?;
    }
    let output = Command::new("git")
        .args(["worktree", "add"])
        .arg(path)
        .args(["-b", branch, base_commit])
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

fn ensure_worktree_parent_is_ignored(
    owner_worktree: &Path,
    worktree_path: &Path,
) -> Result<(), WorktreeError> {
    let Ok(relative) = worktree_path.strip_prefix(owner_worktree) else {
        return Ok(());
    };
    if relative
        .components()
        .next()
        .and_then(|component| component.as_os_str().to_str())
        != Some(".worktrees")
    {
        return Ok(());
    }
    let git_dir = git(owner_worktree, &["rev-parse", "--git-dir"])?
        .trim()
        .to_owned();
    let git_dir = PathBuf::from(git_dir);
    let git_dir = if git_dir.is_absolute() {
        git_dir
    } else {
        owner_worktree.join(git_dir)
    };
    let info_dir = git_dir.join("info");
    std::fs::create_dir_all(&info_dir).map_err(|error| WorktreeError::Git {
        message: error.to_string(),
    })?;
    let exclude_path = info_dir.join("exclude");
    let existing = std::fs::read_to_string(&exclude_path).unwrap_or_default();
    if existing.lines().any(|line| line.trim() == "/.worktrees/") {
        return Ok(());
    }
    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str("/.worktrees/\n");
    std::fs::write(exclude_path, updated).map_err(|error| WorktreeError::Git {
        message: error.to_string(),
    })
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
