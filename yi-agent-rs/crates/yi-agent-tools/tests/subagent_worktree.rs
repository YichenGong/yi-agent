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

#[test]
fn child_creation_rejects_reusing_the_parent_worktree_path() {
    let (repo, head) = repository();
    let service = WorktreeService::new();

    assert!(matches!(
        service.create_child(repo.path(), &head, "child/reused-path", repo.path()),
        Err(WorktreeError::ParentPathReuse { .. })
    ));
}

#[test]
fn child_worktree_is_created_from_the_recorded_parent_commit() {
    let (repo, head) = repository();
    let child_root = TempDir::new().unwrap();
    let child_path = child_root.path().join("child-worktree");
    let service = WorktreeService::new();

    let child = service
        .create_child(repo.path(), &head, "child/task", &child_path)
        .unwrap();

    assert_eq!(child.base_commit, head);
    assert_eq!(child.branch, "child/task");
    assert_eq!(git(&child_path, &["rev-parse", "HEAD"]), child.base_commit);
}

#[test]
fn child_base_must_be_the_parent_current_head_and_is_recorded_as_a_sha() {
    let (repo, head) = repository();
    let service = WorktreeService::new();
    let stale_commit = git(repo.path(), &["rev-parse", "HEAD"]);
    std::fs::write(repo.path().join("README.md"), "new parent head\n").unwrap();
    git(repo.path(), &["add", "README.md"]);
    git(repo.path(), &["commit", "-m", "new parent head"]);

    assert!(matches!(
        service.validate_parent_base(repo.path(), &stale_commit),
        Err(WorktreeError::BaseIsNotParentHead { .. })
    ));

    let child_root = TempDir::new().unwrap();
    let child = service
        .create_child(
            repo.path(),
            "HEAD",
            "child/resolved-base",
            &child_root.path().join("child"),
        )
        .unwrap();
    assert_ne!(child.base_commit, "HEAD");
    assert_eq!(child.base_commit, git(repo.path(), &["rev-parse", "HEAD"]));
    assert_ne!(head, child.base_commit);
}

#[test]
fn delivery_inspection_rejects_an_empty_child_branch() {
    let (repo, head) = repository();
    let child_root = TempDir::new().unwrap();
    let service = WorktreeService::new();
    let child = service
        .create_child(
            repo.path(),
            &head,
            "child/empty-delivery",
            &child_root.path().join("child"),
        )
        .unwrap();

    assert!(matches!(
        service.inspect_delivery(&child),
        Err(WorktreeError::EmptyDelivery { .. })
    ));
}

#[test]
fn dirty_child_worktree_is_not_removed() {
    let (repo, head) = repository();
    let child_root = TempDir::new().unwrap();
    let child_path = child_root.path().join("dirty-child-worktree");
    let service = WorktreeService::new();
    service
        .create_child(repo.path(), &head, "child/dirty", &child_path)
        .unwrap();
    std::fs::write(child_path.join("uncommitted.txt"), "keep me\n").unwrap();

    assert!(matches!(
        service.remove_clean(repo.path(), &child_path),
        Err(WorktreeError::DirtyChild { .. })
    ));
    assert!(child_path.exists());
}

#[test]
fn accepted_child_merges_only_into_its_recorded_direct_parent() {
    let (repo, head) = repository();
    let child_root = TempDir::new().unwrap();
    let child_path = child_root.path().join("accepted-child");
    let service = WorktreeService::new();
    let child = service
        .create_child(repo.path(), &head, "child/accepted", &child_path)
        .unwrap();

    git(&child_path, &["config", "user.email", "test@example.com"]);
    git(&child_path, &["config", "user.name", "Test"]);
    std::fs::write(child_path.join("delivery.txt"), "delivered\n").unwrap();
    git(&child_path, &["add", "delivery.txt"]);
    git(&child_path, &["commit", "-m", "delivery"]);
    let delivery = service.inspect_delivery(&child).unwrap();

    git(repo.path(), &["switch", "-c", "other-parent"]);
    assert!(matches!(
        service.merge_accepted(repo.path(), &child, &delivery, "accept delivery"),
        Err(WorktreeError::WrongParentBranch { .. })
    ));

    git(repo.path(), &["switch", "main"]);
    service
        .merge_accepted(repo.path(), &child, &delivery, "accept delivery")
        .unwrap();
    assert_eq!(
        git(repo.path(), &["show", "HEAD:delivery.txt"]),
        "delivered"
    );
}

#[test]
fn clean_accepted_child_can_be_removed_after_direct_parent_merge() {
    let (repo, head) = repository();
    let child_root = TempDir::new().unwrap();
    let child_path = child_root.path().join("clean-accepted-child");
    let service = WorktreeService::new();
    let child = service
        .create_child(repo.path(), &head, "child/clean-accepted", &child_path)
        .unwrap();

    git(&child_path, &["config", "user.email", "test@example.com"]);
    git(&child_path, &["config", "user.name", "Test"]);
    std::fs::write(child_path.join("delivery.txt"), "delivered\n").unwrap();
    git(&child_path, &["add", "delivery.txt"]);
    git(&child_path, &["commit", "-m", "delivery"]);
    let delivery = service.inspect_delivery(&child).unwrap();
    service
        .merge_accepted(repo.path(), &child, &delivery, "accept delivery")
        .unwrap();

    service.remove_accepted_clean(repo.path(), &child).unwrap();
    assert!(!child_path.exists());
    assert!(
        !Command::new("git")
            .args([
                "show-ref",
                "--verify",
                "--quiet",
                "refs/heads/child/clean-accepted"
            ])
            .current_dir(repo.path())
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn inspected_delivery_pins_the_exact_reviewed_head_for_merge() {
    let (repo, head) = repository();
    let child_root = TempDir::new().unwrap();
    let child_path = child_root.path().join("pinned-head-child");
    let service = WorktreeService::new();
    let child = service
        .create_child(repo.path(), &head, "child/pinned-head", &child_path)
        .unwrap();
    git(&child_path, &["config", "user.email", "test@example.com"]);
    git(&child_path, &["config", "user.name", "Test"]);

    std::fs::write(child_path.join("reviewed.txt"), "reviewed\n").unwrap();
    git(&child_path, &["add", "reviewed.txt"]);
    git(&child_path, &["commit", "-m", "reviewed delivery"]);
    let delivery = service.inspect_delivery(&child).unwrap();
    assert!(delivery.clean);
    assert_eq!(delivery.head_commit.len(), 40);

    std::fs::write(child_path.join("unreviewed.txt"), "unreviewed\n").unwrap();
    git(&child_path, &["add", "unreviewed.txt"]);
    git(&child_path, &["commit", "-m", "unreviewed followup"]);

    assert!(matches!(
        service.merge_inspected_delivery(
            repo.path(),
            &child,
            &delivery,
            "accept reviewed delivery"
        ),
        Err(WorktreeError::ReviewedHeadChanged { .. })
    ));
}

#[test]
fn inspected_delivery_rejects_a_dirty_child_before_merging() {
    let (repo, head) = repository();
    let child_root = TempDir::new().unwrap();
    let child_path = child_root.path().join("dirty-after-review-child");
    let service = WorktreeService::new();
    let child = service
        .create_child(repo.path(), &head, "child/dirty-after-review", &child_path)
        .unwrap();
    git(&child_path, &["config", "user.email", "test@example.com"]);
    git(&child_path, &["config", "user.name", "Test"]);
    std::fs::write(child_path.join("delivery.txt"), "delivered\n").unwrap();
    git(&child_path, &["add", "delivery.txt"]);
    git(&child_path, &["commit", "-m", "delivery"]);
    let delivery = service.inspect_delivery(&child).unwrap();
    std::fs::write(child_path.join("uncommitted.txt"), "not reviewed\n").unwrap();

    assert!(matches!(
        service.merge_accepted(repo.path(), &child, &delivery, "accept delivery"),
        Err(WorktreeError::DirtyChild { .. })
    ));
}
