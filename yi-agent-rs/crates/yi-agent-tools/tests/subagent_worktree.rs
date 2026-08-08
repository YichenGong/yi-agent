use std::process::Command;

use tempfile::TempDir;
use yi_agent_tools::worktree::{WorktreeError, WorktreeService};

fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

fn repository() -> (TempDir, String) {
    let repo = TempDir::new().unwrap();
    git(repo.path(), &["init", "-b", "main"]);
    git(repo.path(), &["config", "user.email", "test@example.com"]);
    git(repo.path(), &["config", "user.name", "Test"]);
    std::fs::write(repo.path().join("README.md"), "base\n").unwrap();
    git(repo.path(), &["add", "README.md"]);
    git(repo.path(), &["commit", "-m", "base"]);
    let head = git(repo.path(), &["rev-parse", "HEAD"]);
    (repo, head)
}

#[test]
fn child_creation_requires_a_clean_committed_parent_base() {
    let (repo, head) = repository();
    let service = WorktreeService::new();
    service.validate_parent_base(repo.path(), &head).unwrap();

    std::fs::write(repo.path().join("README.md"), "dirty\n").unwrap();
    assert!(matches!(
        service.validate_parent_base(repo.path(), &head),
        Err(WorktreeError::DirtyParent { .. })
    ));
}
