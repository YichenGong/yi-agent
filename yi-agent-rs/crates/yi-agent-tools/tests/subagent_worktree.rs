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
fn root_and_child_worktrees_are_distinct_and_begin_at_the_recorded_parent_head() {
    let (repo, _head) = repository();
    let service = WorktreeService::new();
    let root_path = repo.path().join(".worktrees/yi-root");
    let child_path = repo.path().join(".worktrees/yi-child");
    let original_head = git(repo.path(), &["rev-parse", "HEAD"]);
    let original_status = git(repo.path(), &["status", "--porcelain"]);

    let root = service
        .create_root(repo.path(), "feat/yi-root", &root_path)
        .unwrap();
    let child = service
        .create_child(&root.path, &root.base_commit, "feat/yi-child", &child_path)
        .unwrap();

    assert_ne!(root.path, child.path);
    assert_eq!(child.parent_branch, root.branch);
    assert_eq!(child.base_commit, git(&root.path, &["rev-parse", "HEAD"]));
    assert_eq!(git(repo.path(), &["rev-parse", "HEAD"]), root.base_commit);
    assert_eq!(git(repo.path(), &["rev-parse", "HEAD"]), original_head);
    assert_eq!(
        git(repo.path(), &["status", "--porcelain"]),
        original_status
    );
}

#[test]
fn root_creation_requires_a_clean_committed_parent_base() {
    let (repo, _head) = repository();
    let service = WorktreeService::new();
    std::fs::write(repo.path().join("README.md"), "dirty root\n").unwrap();

    assert!(matches!(
        service.create_root(
            repo.path(),
            "feat/yi-root-dirty",
            &repo.path().join(".worktrees/yi-root-dirty")
        ),
        Err(WorktreeError::DirtyParent { .. })
    ));
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
fn root_creation_rejects_a_detached_parent_before_creating_worktree() {
    let (repo, head) = repository();
    let service = WorktreeService::new();
    git(repo.path(), &["checkout", "--detach", &head]);
    let root_path = repo.path().join(".worktrees/detached-root");

    assert!(matches!(
        service.create_root(repo.path(), "feat/detached-root", &root_path),
        Err(WorktreeError::DetachedParent { .. })
    ));
    assert!(!root_path.exists());
}

#[test]
fn child_creation_rejects_a_detached_parent_before_creating_worktree() {
    let (repo, head) = repository();
    let service = WorktreeService::new();
    git(repo.path(), &["checkout", "--detach", &head]);
    let child_path = repo.path().join(".worktrees/detached-child");

    assert!(matches!(
        service.create_child(repo.path(), &head, "feat/detached-child", &child_path),
        Err(WorktreeError::DetachedParent { .. })
    ));
    assert!(!child_path.exists());
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
fn creation_rejects_an_existing_child_path_before_creating_branch() {
    let (repo, head) = repository();
    let service = WorktreeService::new();
    let child_path = repo.path().join(".worktrees/stale-path");
    std::fs::create_dir_all(&child_path).unwrap();

    assert!(matches!(
        service.create_child(repo.path(), &head, "feat/stale-path", &child_path),
        Err(WorktreeError::ExistingWorktreePath { .. })
    ));
    assert!(
        !Command::new("git")
            .args([
                "show-ref",
                "--verify",
                "--quiet",
                "refs/heads/feat/stale-path"
            ])
            .current_dir(repo.path())
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn creation_rejects_an_existing_branch_before_creating_worktree() {
    let (repo, head) = repository();
    let service = WorktreeService::new();
    git(repo.path(), &["branch", "feat/existing-branch"]);
    let child_path = repo.path().join(".worktrees/existing-branch");

    assert!(matches!(
        service.create_child(repo.path(), &head, "feat/existing-branch", &child_path),
        Err(WorktreeError::ExistingBranch { .. })
    ));
    assert!(!child_path.exists());
}

#[test]
fn remove_created_deletes_unmerged_worktree_and_branch() {
    let (repo, head) = repository();
    let service = WorktreeService::new();
    let child_path = repo.path().join(".worktrees/orphaned-child");
    service
        .create_child(repo.path(), &head, "feat/orphaned-child", &child_path)
        .unwrap();

    service
        .remove_created(repo.path(), &child_path, "feat/orphaned-child")
        .unwrap();

    assert!(!child_path.exists());
    assert!(
        !Command::new("git")
            .args([
                "show-ref",
                "--verify",
                "--quiet",
                "refs/heads/feat/orphaned-child"
            ])
            .current_dir(repo.path())
            .status()
            .unwrap()
            .success()
    );
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

#[test]
fn missing_child_worktree_cannot_be_inspected_or_merged() {
    let (repo, head) = repository();
    let service = WorktreeService::new();
    let child_path = repo.path().join(".worktrees/missing-child");
    let child = service
        .create_child(repo.path(), &head, "child/missing", &child_path)
        .unwrap();
    git(&child_path, &["config", "user.email", "test@example.com"]);
    git(&child_path, &["config", "user.name", "Test"]);
    std::fs::write(child_path.join("delivery.txt"), "delivered\n").unwrap();
    git(&child_path, &["add", "delivery.txt"]);
    git(&child_path, &["commit", "-m", "delivery"]);
    let parent_head = git(repo.path(), &["rev-parse", "HEAD"]);

    git(
        repo.path(),
        &[
            "worktree",
            "remove",
            "--force",
            child_path.to_str().unwrap(),
        ],
    );
    assert!(service.inspect_delivery(&child).is_err());
    assert_eq!(git(repo.path(), &["rev-parse", "HEAD"]), parent_head);
}

#[test]
fn accepted_delivery_is_visible_in_parent_history_once() {
    let (repo, head) = repository();
    let child_root = TempDir::new().unwrap();
    let child_path = child_root.path().join("history-child");
    let service = WorktreeService::new();
    let child = service
        .create_child(repo.path(), &head, "child/history", &child_path)
        .unwrap();
    git(&child_path, &["config", "user.email", "test@example.com"]);
    git(&child_path, &["config", "user.name", "Test"]);
    std::fs::write(child_path.join("accepted.txt"), "accepted\n").unwrap();
    git(&child_path, &["add", "accepted.txt"]);
    git(&child_path, &["commit", "-m", "accepted delivery"]);
    let delivery = service.inspect_delivery(&child).unwrap();
    let parent_before = git(repo.path(), &["rev-parse", "HEAD"]);

    service
        .merge_inspected_delivery(repo.path(), &child, &delivery, "accept delivery")
        .unwrap();
    let parent_after = git(repo.path(), &["rev-parse", "HEAD"]);
    assert_ne!(parent_after, parent_before);
    assert_eq!(git(repo.path(), &["show", "HEAD:accepted.txt"]), "accepted");
    git(
        repo.path(),
        &["merge-base", "--is-ancestor", &delivery.head_commit, "HEAD"],
    );
    assert!(
        service
            .merge_inspected_delivery(repo.path(), &child, &delivery, "accept again")
            .is_err()
    );
    assert_eq!(git(repo.path(), &["rev-parse", "HEAD"]), parent_after);
}

#[test]
fn unaccepted_delivery_leaves_parent_history_unchanged() {
    let (repo, head) = repository();
    let child_root = TempDir::new().unwrap();
    let child_path = child_root.path().join("unaccepted-child");
    let service = WorktreeService::new();
    let child = service
        .create_child(repo.path(), &head, "child/unaccepted", &child_path)
        .unwrap();
    git(&child_path, &["config", "user.email", "test@example.com"]);
    git(&child_path, &["config", "user.name", "Test"]);
    std::fs::write(child_path.join("rejected.txt"), "not accepted\n").unwrap();
    git(&child_path, &["add", "rejected.txt"]);
    git(&child_path, &["commit", "-m", "unaccepted delivery"]);
    let delivery = service.inspect_delivery(&child).unwrap();
    let parent_head = git(repo.path(), &["rev-parse", "HEAD"]);
    let parent_readme = git(repo.path(), &["show", "HEAD:README.md"]);

    assert!(delivery.clean);
    assert_eq!(git(repo.path(), &["rev-parse", "HEAD"]), parent_head);
    assert_eq!(git(repo.path(), &["show", "HEAD:README.md"]), parent_readme);
    assert!(
        !Command::new("git")
            .args(["cat-file", "-e", "HEAD:rejected.txt"])
            .current_dir(repo.path())
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn accepted_delivery_cannot_be_merged_twice_and_dirty_cleanup_is_refused() {
    let (repo, head) = repository();
    let service = WorktreeService::new();
    let child_path = repo.path().join(".worktrees/one-merge");
    let child = service
        .create_child(repo.path(), &head, "child/one-merge", &child_path)
        .unwrap();
    git(&child_path, &["config", "user.email", "test@example.com"]);
    git(&child_path, &["config", "user.name", "Test"]);
    std::fs::write(child_path.join("delivery.txt"), "delivered\n").unwrap();
    git(&child_path, &["add", "delivery.txt"]);
    git(&child_path, &["commit", "-m", "delivery"]);
    let delivery = service.inspect_delivery(&child).unwrap();

    service
        .merge_inspected_delivery(repo.path(), &child, &delivery, "accept delivery")
        .unwrap();
    let parent_head = git(repo.path(), &["rev-parse", "HEAD"]);
    assert!(
        service
            .merge_inspected_delivery(repo.path(), &child, &delivery, "accept delivery again")
            .is_err()
    );
    assert_eq!(git(repo.path(), &["rev-parse", "HEAD"]), parent_head);

    std::fs::write(child_path.join("uncommitted.txt"), "retain me\n").unwrap();
    assert!(matches!(
        service.remove_accepted_clean(repo.path(), &child),
        Err(WorktreeError::DirtyChild { .. })
    ));
    assert!(child_path.exists());
}

#[test]
fn contains_commit_reports_whether_a_revision_is_an_ancestor_of_head() {
    let (repo, base) = repository();
    let service = WorktreeService::new();
    let root_path = repo.path().join(".worktrees/yi-contains");
    let root = service
        .create_root(repo.path(), "feat/yi-contains", &root_path)
        .unwrap();

    assert!(service.contains_commit(&root.path, &base).unwrap());

    let child_path = repo.path().join(".worktrees/yi-contains-child");
    let child = service
        .create_child(
            &root.path,
            &root.base_commit,
            "feat/yi-contains-child",
            &child_path,
        )
        .unwrap();
    std::fs::write(child.path.join("delivery.txt"), "ready\n").unwrap();
    git(&child.path, &["add", "delivery.txt"]);
    git(&child.path, &["commit", "-m", "child delivery"]);
    let child_head = git(&child.path, &["rev-parse", "HEAD"]);

    // The child commit exists but is not yet in the parent's HEAD.
    assert!(!service.contains_commit(&root.path, &child_head).unwrap());

    git(
        &root.path,
        &["merge", "--no-ff", &child_head, "-m", "integrate"],
    );
    assert!(service.contains_commit(&root.path, &child_head).unwrap());
}

#[test]
fn reclaim_directory_removes_only_the_directory_and_keeps_the_branch() {
    let (repo, _base) = repository();
    let service = WorktreeService::new();
    let root_path = repo.path().join(".worktrees/yi-reclaim-root");
    let child_path = repo.path().join(".worktrees/yi-reclaim-child");
    let root = service
        .create_root(repo.path(), "feat/yi-reclaim-root", &root_path)
        .unwrap();
    let child = service
        .create_child(
            &root.path,
            &root.base_commit,
            "feat/yi-reclaim-child",
            &child_path,
        )
        .unwrap();
    std::fs::write(child.path.join("delivery.txt"), "ready\n").unwrap();
    git(&child.path, &["add", "delivery.txt"]);
    git(&child.path, &["commit", "-m", "child delivery"]);
    let delivered = git(&child.path, &["rev-parse", "HEAD"]);

    service.reclaim_directory(repo.path(), &child.path).unwrap();

    assert!(!child.path.exists(), "directory is gone");
    assert_eq!(
        git(repo.path(), &["rev-parse", "feat/yi-reclaim-child"]),
        delivered,
        "branch ref still pins the delivered commit"
    );
    assert_eq!(
        git(repo.path(), &["show", "feat/yi-reclaim-child:delivery.txt"]),
        "ready",
        "committed content is still recoverable"
    );
}

#[test]
fn reclaim_directory_refuses_a_dirty_worktree() {
    let (repo, _base) = repository();
    let service = WorktreeService::new();
    let root_path = repo.path().join(".worktrees/yi-dirty-root");
    let child_path = repo.path().join(".worktrees/yi-dirty-child");
    let root = service
        .create_root(repo.path(), "feat/yi-dirty-root", &root_path)
        .unwrap();
    let child = service
        .create_child(
            &root.path,
            &root.base_commit,
            "feat/yi-dirty-child",
            &child_path,
        )
        .unwrap();
    std::fs::write(child.path.join("scratch.txt"), "uncommitted\n").unwrap();

    let error = service
        .reclaim_directory(repo.path(), &child.path)
        .unwrap_err();

    assert!(
        matches!(error, WorktreeError::DirtyChild { .. }),
        "unexpected error: {error}"
    );
    assert!(child.path.exists(), "dirty worktree is left in place");
}

#[test]
fn reattach_worktree_attaches_an_existing_branch() {
    let (repo, _base) = repository();
    let service = WorktreeService::new();
    let root_path = repo.path().join(".worktrees/yi-reattach-root");
    let child_path = repo.path().join(".worktrees/yi-reattach-child");
    let root = service
        .create_root(repo.path(), "feat/yi-reattach-root", &root_path)
        .unwrap();
    let child = service
        .create_child(
            &root.path,
            &root.base_commit,
            "feat/yi-reattach-child",
            &child_path,
        )
        .unwrap();
    std::fs::write(child.path.join("delivery.txt"), "ready\n").unwrap();
    git(&child.path, &["add", "delivery.txt"]);
    git(&child.path, &["commit", "-m", "child delivery"]);
    let delivered = git(&child.path, &["rev-parse", "HEAD"]);

    service.reclaim_directory(repo.path(), &child.path).unwrap();
    assert!(!child.path.exists());

    service
        .reattach_worktree(repo.path(), &child.path, &child.branch)
        .unwrap();

    assert!(child.path.exists(), "worktree is restored");
    assert_eq!(
        git(&child.path, &["rev-parse", "HEAD"]),
        delivered,
        "restored tip equals the pre-reclaim tip"
    );
    assert_eq!(
        git(&child.path, &["rev-parse", "--abbrev-ref", "HEAD"]),
        child.branch
    );
    assert_eq!(
        std::fs::read_to_string(child.path.join("delivery.txt")).unwrap(),
        "ready\n"
    );
}

#[test]
fn reattach_worktree_fails_when_the_branch_is_absent() {
    let (repo, _base) = repository();
    let service = WorktreeService::new();
    let root_path = repo.path().join(".worktrees/yi-nobranch-root");
    let root = service
        .create_root(repo.path(), "feat/yi-nobranch-root", &root_path)
        .unwrap();
    let missing = repo.path().join(".worktrees/yi-nobranch-child");

    let error = service
        .reattach_worktree(repo.path(), &missing, "feat/does-not-exist")
        .unwrap_err();

    assert!(
        matches!(error, WorktreeError::Git { .. }),
        "unexpected error: {error}"
    );
    assert!(!missing.exists());
    let _ = root;
}

#[test]
fn is_ancestor_distinguishes_merged_from_unmerged_branches() {
    let (repo, base) = repository();
    let service = WorktreeService::new();
    let root_path = repo.path().join(".worktrees/yi-ancestor-root");
    let child_path = repo.path().join(".worktrees/yi-ancestor-child");
    let root = service
        .create_root(repo.path(), "feat/yi-ancestor-root", &root_path)
        .unwrap();
    let child = service
        .create_child(
            &root.path,
            &root.base_commit,
            "feat/yi-ancestor-child",
            &child_path,
        )
        .unwrap();
    std::fs::write(child.path.join("delivery.txt"), "ready\n").unwrap();
    git(&child.path, &["add", "delivery.txt"]);
    git(&child.path, &["commit", "-m", "child delivery"]);

    assert!(
        !service
            .is_ancestor(&root.path, &child.branch, &root.branch)
            .unwrap(),
        "an unmerged child branch is not an ancestor"
    );

    git(
        &root.path,
        &["merge", "--no-ff", &child.branch, "-m", "integrate"],
    );
    assert!(
        service
            .is_ancestor(&root.path, &child.branch, &root.branch)
            .unwrap(),
        "a merged child branch is an ancestor"
    );

    assert!(
        service.is_ancestor(&root.path, &base, "HEAD").unwrap(),
        "the base commit is always an ancestor of HEAD"
    );
}

#[test]
fn ignoring_a_project_local_runtime_directory_keeps_the_checkout_clean() {
    let (repo, _head) = repository();
    let service = WorktreeService::new();
    let runtime_dir = repo.path().join(".yi-agent/runtime");
    std::fs::create_dir_all(&runtime_dir).unwrap();
    std::fs::write(runtime_dir.join("runtime.sqlite"), "state").unwrap();
    assert!(
        !git(repo.path(), &["status", "--porcelain"]).is_empty(),
        "precondition: the project-local runtime dir must dirty the checkout"
    );

    service
        .ignore_inside_repository(repo.path(), &runtime_dir)
        .unwrap();

    assert_eq!(
        git(repo.path(), &["status", "--porcelain"]),
        "",
        "the self-created runtime dir must not leave the checkout dirty"
    );
    // The ignore lives in the shared, untracked exclude file, never in the
    // user's `.gitignore` or any tracked file.
    assert!(
        !repo.path().join(".gitignore").exists(),
        "no .gitignore may be invented for the user"
    );
}

#[test]
fn ignoring_an_already_ignored_runtime_directory_is_idempotent() {
    let (repo, _head) = repository();
    let service = WorktreeService::new();
    let runtime_dir = repo.path().join(".yi-agent/runtime");
    std::fs::create_dir_all(&runtime_dir).unwrap();

    service
        .ignore_inside_repository(repo.path(), &runtime_dir)
        .unwrap();
    service
        .ignore_inside_repository(repo.path(), &runtime_dir)
        .unwrap();

    let exclude = std::fs::read_to_string(repo.path().join(".git/info/exclude")).unwrap();
    assert_eq!(
        exclude.matches("/.yi-agent/").count(),
        1,
        "repeated calls must not duplicate the entry: {exclude}"
    );
}

#[test]
fn ignoring_a_path_outside_the_repository_is_a_no_op() {
    let (repo, _head) = repository();
    let service = WorktreeService::new();
    let outside = TempDir::new().unwrap();

    service
        .ignore_inside_repository(repo.path(), outside.path())
        .unwrap();

    let exclude =
        std::fs::read_to_string(repo.path().join(".git/info/exclude")).unwrap_or_default();
    assert!(
        !exclude.contains("tmp"),
        "an out-of-repo path must not be written: {exclude}"
    );
}

#[test]
fn ignoring_a_nested_project_state_uses_a_repository_rooted_entry() {
    let (repo, _head) = repository();
    let service = WorktreeService::new();
    // A workdir may be a subdirectory of the repository: the ignore entry must
    // be anchored at the repository root, not at the subdirectory.
    let nested = repo.path().join("nested/project");
    std::fs::create_dir_all(nested.join(".yi-agent/runtime")).unwrap();
    std::fs::write(nested.join(".yi-agent/runtime/runtime.sqlite"), "state").unwrap();

    service
        .ignore_project_path(&nested.join(".yi-agent"))
        .unwrap();

    assert_eq!(
        git(repo.path(), &["status", "--porcelain"]),
        "",
        "a nested runtime state dir must still leave the repository clean"
    );
    let exclude = std::fs::read_to_string(repo.path().join(".git/info/exclude")).unwrap();
    assert!(
        exclude.contains("/nested/project/.yi-agent/"),
        "the entry must be repository-rooted, got: {exclude}"
    );
}

#[test]
fn ignoring_a_project_path_resolves_the_repository_root_itself() {
    let (repo, _head) = repository();
    let service = WorktreeService::new();
    let runtime_dir = repo.path().join(".yi-agent/runtime");
    std::fs::create_dir_all(&runtime_dir).unwrap();
    std::fs::write(runtime_dir.join("runtime.sqlite"), "state").unwrap();

    // The caller passes the state dir directly and need not know the repo root.
    service
        .ignore_project_path(&repo.path().join(".yi-agent"))
        .unwrap();

    assert_eq!(git(repo.path(), &["status", "--porcelain"]), "");
}

#[test]
fn ignoring_a_not_yet_created_nested_state_dir_leaves_the_repository_clean() {
    let (repo, _head) = repository();
    let service = WorktreeService::new();
    // The daemon hook runs before the state dir exists: the entry must still be
    // recorded, and it must be rooted at the repository, not the subdirectory.
    let nested = repo.path().join("nested/project");
    std::fs::create_dir_all(&nested).unwrap();

    service
        .ignore_project_path(&nested.join(".yi-agent"))
        .unwrap();

    let exclude = std::fs::read_to_string(repo.path().join(".git/info/exclude")).unwrap();
    assert!(
        exclude.contains("/nested/project/.yi-agent/"),
        "a missing state dir must still get a repository-rooted entry, got: {exclude}"
    );

    // Once the daemon creates its state, the checkout stays clean.
    let runtime = nested.join(".yi-agent/runtime");
    std::fs::create_dir_all(&runtime).unwrap();
    std::fs::write(runtime.join("runtime.sqlite"), "state").unwrap();
    assert_eq!(git(repo.path(), &["status", "--porcelain"]), "");
}

#[test]
fn ignoring_works_inside_a_linked_git_worktree() {
    let (repo, _head) = repository();
    let service = WorktreeService::new();
    // A linked worktree has its own gitdir, but git reads `info/exclude` from
    // the shared common dir. Writing to the per-worktree gitdir would leave the
    // checkout dirty, which is exactly this repository's own `.worktrees/` layout.
    let linked = repo.path().join(".worktrees/feature");
    git(
        repo.path(),
        &[
            "worktree",
            "add",
            "-q",
            linked.to_str().unwrap(),
            "-b",
            "feature",
        ],
    );
    let runtime = linked.join(".yi-agent/runtime");
    std::fs::create_dir_all(&runtime).unwrap();
    std::fs::write(runtime.join("runtime.sqlite"), "state").unwrap();
    assert!(
        !git(&linked, &["status", "--porcelain"]).is_empty(),
        "precondition: the runtime state must dirty the linked worktree"
    );

    service
        .ignore_project_path(&linked.join(".yi-agent"))
        .unwrap();

    assert_eq!(
        git(&linked, &["status", "--porcelain"]),
        "",
        "a linked worktree must also end up clean"
    );
}
