use std::path::{Path, PathBuf};
use std::process::Command;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum WorktreeError {
    #[error("parent worktree is dirty: {path}")]
    DirtyParent { path: PathBuf },
    #[error("parent base commit is not available: {base}")]
    UnknownBase { base: String },
    #[error("git command failed: {message}")]
    Git { message: String },
}

#[derive(Debug, Default)]
pub struct WorktreeService;

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
