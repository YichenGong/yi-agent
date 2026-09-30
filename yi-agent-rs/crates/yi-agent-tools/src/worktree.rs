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
    #[error("child worktree is dirty: {path}")]
    DirtyWorkdir { path: PathBuf },
    #[error("child delivery has no commits beyond its recorded base {base}")]
    EmptyDelivery { base: String },
    #[error("child delivery has no commits beyond its recorded base {base}")]
    NoCommitsBeyond { path: PathBuf, base: String },
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

/// The facts a workdir reports about its own delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkdirDelivery {
    pub branch: String,
    pub base_commit: String,
    pub head_commit: String,
    pub clean: bool,
}

impl WorktreeService {
    pub fn new() -> Self {
        Self
    }

    /// Reads a workdir's delivery facts with plain git probes.
    ///
    /// Unlike worktree orchestration, this requires nothing of the directory
    /// beyond being a git checkout: the parent may have created it by any means.
    /// An uncommitted change is [`WorktreeError::DirtyWorkdir`] and a HEAD equal
    /// to `base` is [`WorktreeError::NoCommitsBeyond`]; those are the only two
    /// conditions a delivery cannot be reviewed with.
    pub fn inspect_workdir(
        &self,
        workdir: &Path,
        base: &str,
    ) -> Result<WorkdirDelivery, WorktreeError> {
        let head_commit = git(workdir, &["rev-parse", "HEAD"])?.trim().to_owned();
        if !git(workdir, &["status", "--porcelain"])?.trim().is_empty() {
            return Err(WorktreeError::DirtyWorkdir {
                path: workdir.to_path_buf(),
            });
        }
        if head_commit == base {
            return Err(WorktreeError::NoCommitsBeyond {
                path: workdir.to_path_buf(),
                base: base.to_owned(),
            });
        }
        Ok(WorkdirDelivery {
            branch: current_branch(workdir).unwrap_or_default(),
            base_commit: base.to_owned(),
            head_commit,
            clean: true,
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
