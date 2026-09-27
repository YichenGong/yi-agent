use std::path::{Path, PathBuf};
use std::process::Command;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum WorktreeError {
    #[error(
        "parent worktree is dirty: {path} — a coding root needs a clean checkout; \
         commit or stash the changes first"
    )]
    DirtyParent { path: PathBuf },
    #[error("parent worktree is detached: {path}")]
    DetachedParent { path: PathBuf },
    #[error("child worktree path reuses its parent: {path}")]
    ParentPathReuse { path: PathBuf },
    #[error("worktree path already exists: {path}")]
    ExistingWorktreePath { path: PathBuf },
    #[error("worktree branch already exists: {branch}")]
    ExistingBranch { branch: String },
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
        ensure_worktree_target_available(repository_root, root_path, branch)?;
        ensure_worktree_parent_is_ignored(repository_root, root_path)?;
        add_worktree(repository_root, root_path, branch, &base_commit)?;
        Ok(ChildWorktree {
            path: root_path.to_path_buf(),
            branch: branch.to_owned(),
            parent_branch,
            base_commit,
        })
    }

    /// Marks a repo-relative path as git-ignored through `.git/info/exclude`.
    ///
    /// The runtime lives inside the project (`<workdir>/.yi-agent/`) and
    /// therefore dirties the checkout the moment the daemon creates it. Git
    /// worktree provisioning refuses a dirty parent, so the tool would defeat
    /// its own precondition on the first delegation. Ignoring the path in the
    /// shared, untracked `info/exclude` file removes the self-inflicted dirt
    /// without touching the user's `.gitignore` or any tracked file.
    ///
    /// The entry is anchored at the repository root, so a workdir that is a
    /// subdirectory of its repository resolves to a nested pattern rather than
    /// a wrong top-level one. A path outside the repository, or an
    /// already-listed one, is a no-op.
    pub fn ignore_inside_repository(
        &self,
        repository_root: &Path,
        target: &Path,
    ) -> Result<(), WorktreeError> {
        let root = canonicalize_deepest_existing(repository_root);
        let resolved_target = canonicalize_deepest_existing(target);
        let Ok(relative) = resolved_target.strip_prefix(&root) else {
            return Ok(());
        };
        let Some(entry) = repository_relative_ignore_entry(relative) else {
            return Ok(());
        };
        append_exclude_entry(&root, &entry)
    }

    /// Marks an absolute path as ignored in whichever repository contains it.
    ///
    /// Unlike [`Self::ignore_inside_repository`], this does not assume the
    /// caller already knows the repository root: it resolves the root with git
    /// first, so a workdir nested below the repository top level still yields a
    /// correctly anchored pattern. A path outside any repository is a no-op, so
    /// a non-git workdir never fails and never creates stray files.
    pub fn ignore_project_path(&self, target: &Path) -> Result<(), WorktreeError> {
        let Some(root) = self.containing_worktree_root(target)? else {
            return Ok(());
        };
        self.ignore_inside_repository(&root, target)
    }

    /// Resolves the worktree root that owns `path`, or `None` when `path` is not
    /// inside any git repository.
    ///
    /// `path` is typically the not-yet-created `<workdir>/.yi-agent`, so git is
    /// invoked from the deepest existing ancestor rather than from `path`
    /// itself: spawning a process in a missing directory fails outright.
    fn containing_worktree_root(&self, path: &Path) -> Result<Option<PathBuf>, WorktreeError> {
        let working_directory = deepest_existing_directory(path);
        let output = Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(&working_directory)
            .output()
            .map_err(|error| WorktreeError::Git {
                message: error.to_string(),
            })?;
        if !output.status.success() {
            return Ok(None);
        }
        let root = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if root.is_empty() {
            return Ok(None);
        }
        Ok(Some(PathBuf::from(root)))
    }

    /// Rejects uncommitted parent state before a child branch can be derived.
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
        ensure_worktree_target_available(parent_worktree, child_path, branch)?;
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

    /// Whether `rev` is already contained in the worktree's current HEAD.
    ///
    /// Exit code 0 means ancestor, 1 means not an ancestor, anything else is a
    /// real git error (for example an unknown revision).
    pub fn contains_commit(&self, worktree: &Path, rev: &str) -> Result<bool, WorktreeError> {
        let output = Command::new("git")
            .args(["merge-base", "--is-ancestor", rev, "HEAD"])
            .current_dir(worktree)
            .output()
            .map_err(|error| WorktreeError::Git {
                message: error.to_string(),
            })?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(WorktreeError::Git {
                message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            }),
        }
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

    pub fn remove_created(
        &self,
        owner_worktree: &Path,
        child_path: &Path,
        branch: &str,
    ) -> Result<(), WorktreeError> {
        let output = Command::new("git")
            .args(["worktree", "remove", "--force"])
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
        let output = Command::new("git")
            .args(["branch", "-D", branch])
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
    let branch = git(workdir, &["branch", "--show-current"])?
        .trim()
        .to_owned();
    if branch.is_empty() {
        return Err(WorktreeError::DetachedParent {
            path: workdir.to_path_buf(),
        });
    }
    Ok(branch)
}

fn ensure_worktree_target_available(
    owner_worktree: &Path,
    path: &Path,
    branch: &str,
) -> Result<(), WorktreeError> {
    if path.try_exists().map_err(|error| WorktreeError::Git {
        message: error.to_string(),
    })? {
        return Err(WorktreeError::ExistingWorktreePath {
            path: path.to_path_buf(),
        });
    }
    let branch_ref = format!("refs/heads/{branch}");
    let status = Command::new("git")
        .args(["show-ref", "--verify", "--quiet", &branch_ref])
        .current_dir(owner_worktree)
        .status()
        .map_err(|error| WorktreeError::Git {
            message: error.to_string(),
        })?;
    if status.success() {
        return Err(WorktreeError::ExistingBranch {
            branch: branch.to_owned(),
        });
    }
    Ok(())
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
    append_exclude_entry(owner_worktree, "/.worktrees/")
}

/// Normalizes a repo-relative path into a rooted `.git/info/exclude` entry.
///
/// A directory entry ends in `/` so git treats the directory itself as
/// ignored, matching the existing `/.worktrees/` convention. Returns `None`
/// for an empty relative path, which has no meaningful ignore entry.
fn repository_relative_ignore_entry(relative: &Path) -> Option<String> {
    if relative.as_os_str().is_empty() {
        return None;
    }
    let mut entry = format!("/{}", relative.to_string_lossy());
    if !entry.ends_with('/') {
        entry.push('/');
    }
    Some(entry)
}

/// Walks up from `path` until an existing directory is found.
///
/// The best parent for spawning `git` when the target itself may not exist yet.
/// Falls back to the original path when nothing along the chain exists.
fn deepest_existing_directory(path: &Path) -> PathBuf {
    let mut candidate = path.to_path_buf();
    loop {
        if candidate.is_dir() {
            return candidate;
        }
        if !candidate.pop() {
            return path.to_path_buf();
        }
    }
}

/// Resolves symlinks as far as the path actually exists, then re-appends the
/// remaining components.
///
/// macOS exposes temporary directories through a `/var` symlink to
/// `/private/var`, and a state directory that does not exist yet cannot be
/// canonicalized at all. Resolving the deepest existing ancestor gives both the
/// repository root and the not-yet-created target a common, comparable form, so
/// a legitimate in-repo path is never misread as "outside the repository".
fn canonicalize_deepest_existing(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut remainder: Vec<std::ffi::OsString> = Vec::new();
    loop {
        match std::fs::canonicalize(&existing) {
            Ok(resolved) => {
                let mut resolved = resolved;
                for component in remainder.iter().rev() {
                    resolved.push(component);
                }
                return resolved;
            }
            Err(_) => match existing.file_name() {
                Some(name) => remainder.push(name.to_os_string()),
                None => return path.to_path_buf(),
            },
        }
        if !existing.pop() {
            return path.to_path_buf();
        }
    }
}

/// Appends one entry to the repository's shared `.git/info/exclude` if absent.
///
/// Writing into `info/exclude` keeps the ignore untracked: it never modifies a
/// user-authored `.gitignore` nor any tracked file in the checkout. The path is
/// resolved with `git rev-parse --git-path info/exclude` rather than
/// `--git-dir`, because a linked worktree has its own gitdir while git reads the
/// exclude file from the shared common dir.
fn append_exclude_entry(repository_root: &Path, entry: &str) -> Result<(), WorktreeError> {
    let exclude_path = git(
        repository_root,
        &["rev-parse", "--git-path", "info/exclude"],
    )?;
    let exclude_path = PathBuf::from(exclude_path.trim());
    let exclude_path = if exclude_path.is_absolute() {
        exclude_path
    } else {
        repository_root.join(exclude_path)
    };
    if let Some(info_dir) = exclude_path.parent() {
        std::fs::create_dir_all(info_dir).map_err(|error| WorktreeError::Git {
            message: error.to_string(),
        })?;
    }
    let existing = std::fs::read_to_string(&exclude_path).unwrap_or_default();
    if existing.lines().any(|line| line.trim() == entry) {
        return Ok(());
    }
    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(entry);
    updated.push('\n');
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
