# Subagent Git Worktree Delivery Design

## References

- [Architecture decision index](2026-08-09-subagent-architecture-design.md)
- Repository worktree/commit rules in `CLAUDE.md`
- Existing shell sandbox: `yi-agent-rs/crates/yi-agent-tools/src/sandbox.rs`

## Ownership

The user checkout is never a delegated write target. A root coding session gets
an integration branch/worktree; every coding child gets a distinct branch and
worktree. Only the lease owner writes its integration branch.

```text
user checkout (read-only)
  root integration branch
    child integration branch
      leaf delivery branch
```

## Base Rule

Every child branch begins at a recorded committed parent HEAD. If a parent has
uncommitted changes, it must first create a self-contained verified baseline
commit, keep dependent work local, or delegate only read-only work. The runtime
rejects a coding spawn with `UncommittedParentBase`; it must not stash or copy
uncommitted files invisibly.

## Delivery Workflow

1. Create child branch/worktree from `parent_base_commit` and persist both.
2. Child edits only that worktree, runs the contract checks, and commits.
3. Child reports base/head/range, clean status, checks, changed files, and notes.
4. Parent inspects the recorded range and chooses accept, rework, or reject.
5. Accept runs `merge --no-ff child_branch` in the parent's worktree, then the
   parent's integration checks. Only then is child delivery completed.

Rework on an unchanged parent HEAD may append commits to the existing branch.
If the parent integration branch moved, the new attempt receives a fresh branch
from the new HEAD; selected old commits may be cherry-picked with provenance.
No attempt rewrites an old branch history.

## Conflicts And Cleanup

Merge conflicts belong to the parent lease owner. It may resolve locally or
request a child rework from a fresh base, but a child never writes the parent
worktree. Failed/rejected/dirty/recovery worktrees are retained. Clean accepted
worktrees may be removed only after parent integration; repository policy may
require immediate removal. Cleanup records branch, worktree, actor, and result.

## Required Tests

- Spawn refuses dirty parent base and parent-worktree path reuse.
- Child base equals recorded parent commit.
- Leaf can merge only into direct child; direct child only into root.
- Rework after parent change uses a new branch and preserves old commits.
- Dirty worktree cleanup is refused; clean accepted worktree cleanup succeeds.

## Naming And Layout

The runtime resolves repository root with `git rev-parse --show-toplevel` and
stores canonical absolute paths. A session receives a stable slug and branch
prefix chosen by project policy; this repository uses conventional prefixes.

```text
.worktrees/feat-subagent-s01-root/       branch feat/subagent-s01-root
.worktrees/feat-subagent-s01-a01/        branch feat/subagent-s01-a01
.worktrees/feat-subagent-s01-a01-a01/    branch feat/subagent-s01-a01-a01
```

The path never uses a raw task title. Supervisor-generated IDs prevent path
traversal and make cleanup targets unambiguous. `WorktreeLease` records repo
root, target branch, parent branch, child branch, base commit, path, creation
event, and current cleanliness.

## Root Session Creation

Before a coding root starts, inspect the user checkout:

```text
clean checkout:  create root branch/worktree from selected target HEAD
dirty checkout:  do not copy/stash changes; create read-only root or require
                 the user to commit/stash/select an explicit base first
non-Git project: coding subagents are disabled until a project adapter exists
```

The target branch is part of the root contract, normally the branch selected by
the user. For this repository, a root derived from `main` must still follow the
project rule: all modifications occur in a worktree, only the reviewed root
branch is merged to `main` with `--no-ff`, then its worktree and branch are
removed.

## Exact Git Operations

All commands execute from a lease owner or trusted worktree service, never from
an agent-composed shell string:

```text
git worktree add <path> -b <child-branch> <parent-base-commit>
git -C <child-path> status --porcelain=v1
git -C <child-path> rev-parse HEAD
git -C <parent-path> merge --no-ff <child-branch> -m <generated-message>
git -C <parent-path> diff --check <parent-base>..HEAD
git worktree remove <child-path>
git branch -d <child-branch>
```

Before merge, verify the child branch merge-base equals the recorded base or is
an allowed descendant in a rework attempt. Before cleanup, verify porcelain is
empty, the branch is merged into its direct parent, and no active task retains
the lease. Failure records command, exit code, stderr summary, and Git status;
it never triggers force removal.

## Delivery Validation

`DeliveryReport` is accepted only when all fields match the runtime's recorded
lease:

```rust
pub struct DeliveryReport {
    pub task_id: TaskId,
    pub attempt_id: AttemptId,
    pub base_commit: CommitId,
    pub head_commit: CommitId,
    pub branch: BranchName,
    pub worktree_clean: bool,
    pub changed_files: Vec<RepoPath>,
    pub verification: Vec<CommandEvidence>,
    pub known_limitations: Vec<String>,
}
```

For a coding contract with `require_commit=true`, `head_commit` must differ from
base unless `CompletedNoChanges` is explicitly permitted. The report's commit
must be reachable from the child branch and must not include unrelated commits
before its recorded base. The parent reviews the diff itself; child-provided
changed-file lists are advisory only.

## Integration Failure And Rework

If parent merge succeeds but integration validation fails, retain the merge
commit and transition the parent to `Blocked(IntegrationValidationFailed)`;
never silently reset the parent branch. The parent may create a corrective child
attempt from its new committed HEAD. If merge conflicts, no merge commit exists;
the parent can resolve in its own worktree, request rework on a new base, or
reject the delivery. The conflict report is attached to the child review event.

Rework feedback always names the prior delivery and records whether the parent
branch changed. A new-base rework uses a new branch; the old worktree is retained
until the new attempt is accepted or a user explicitly cleans it.

## Cleanup Retention

Accepted clean child worktrees are eligible for immediate cleanup when project
policy requires it; otherwise the daemon keeps metadata for 7 days and may
offer cleanup, never perform destructive cleanup of a dirty path. Failed,
rejected, cancelled, and recovery-required worktrees have no automatic deletion
deadline. User cleanup displays the exact branch/path/commit range and requires
confirmation.
