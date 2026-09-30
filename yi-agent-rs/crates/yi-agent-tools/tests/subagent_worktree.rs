use std::process::Command;

use tempfile::TempDir;
use yi_agent_tools::worktree::WorktreeService;

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

#[test]
fn inspect_workdir_reports_head_and_cleanliness_without_a_worktree() {
    let (repo, before) = repository();
    let service = WorktreeService::new();
    std::fs::write(repo.path().join("delivery.txt"), "ready\n").unwrap();
    git(repo.path(), &["add", "delivery.txt"]);
    git(repo.path(), &["commit", "-m", "delivery"]);

    let delivery = service.inspect_workdir(repo.path(), &before).unwrap();
    assert!(delivery.clean, "committed work is clean");
    assert_ne!(delivery.head_commit, before);
    assert_eq!(delivery.base_commit, before);
    assert_eq!(delivery.branch, "main");

    std::fs::write(repo.path().join("delivery.txt"), "not committed\n").unwrap();
    let dirty = service.inspect_workdir(repo.path(), &before).unwrap_err();
    assert!(
        dirty.to_string().contains("child worktree is dirty"),
        "unexpected error: {dirty}"
    );
}

#[test]
fn inspect_workdir_rejects_a_head_equal_to_its_base() {
    let (repo, before) = repository();
    let service = WorktreeService::new();

    let error = service.inspect_workdir(repo.path(), &before).unwrap_err();

    assert!(
        error.to_string().contains("no commits beyond"),
        "unexpected error: {error}"
    );
}
