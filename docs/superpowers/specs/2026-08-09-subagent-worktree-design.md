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
