# Subagent Worktree Recycling Design

## Goal

Make subagent results actually reach the parent branch, and automatically recycle a
child task's worktree, branch, and workspace record once that integration is confirmed.
This stops the unbounded growth of `.worktrees/` directories and `feat/yi-agent-*`
branches that a subagent session currently leaves behind.

## Problem

Observed state: 32 leftover local `feat/yi-agent-*` branches and stale worktrees under
`.worktrees/`. Three root causes:

1. **No production merge exists.** `WorktreeService::merge_accepted`,
   `merge_inspected_delivery`, and `remove_accepted_clean`
   (`crates/yi-agent-tools/src/worktree.rs:169,180,254`) have no non-test callers.
   `RuntimeCoordinator::accept_review` (`crates/yi-agent-store/src/runtime.rs:1859`)
   updates the supervisor and SQLite but never merges, and no production code constructs
   an `IntegrationValidation`. `crates/yi-agent/src/main.rs:1813` deliberately asserts
   `accept_review` is not exposed as a tool, so the trusted completion path is
   unreachable today.
2. **No automatic cleanup exists.** `DaemonWorkspaceService::cleanup_prepared`
   (`crates/yi-agent/src/subagent_runtime.rs:272`) fires only on the workspace-record
   rollback path (`runtime.rs:1502`). `task_workspaces` rows are inserted
   (`crates/yi-agent-store/src/repository.rs:3053`) and never deleted; recovery even
   deliberately retains `worktree:`/`workspace:` leases.
3. **Branch names collide on reuse.** `short()` truncates session/task ids to 8 hex
   chars (`subagent_runtime.rs:295`). Because branches are never deleted,
   `UNIQUE(repository_root, branch)` (`repository.rs:4476`) can reject a later
   workspace that reuses a colliding name.

## Scope

**In scope**

- Wire the trusted completion path so an integrated child delivery completes.
- Recycle the worktree, branch, and `task_workspaces` row for accepted (integrated)
  deliveries only.

**Out of scope** (separate work)

- Recycling worktrees for tasks that never integrate (failed, cancelled, rejected,
  abandoned rework).
- TTL-based sweeping, a manual `gc` command.
- Recycling the root worktree.

## Responsibility Split

The integration *action* and the integration *guarantee* live in different layers. The
prompt drives best-effort behavior; the software layer owns every irreversible or
invariant-bearing step.

| Concern | Layer | Nature |
| --- | --- | --- |
| A1. Merge the delivered commit after a delivery notification | Agent system prompt | Best-effort |
| A2. Verify integration and complete the child | Runtime software | Hard guarantee |
| B. Remove worktree, branch, and workspace row | Runtime software | Hard guarantee |

Rationale:

- **A merge action belongs in the prompt.** The parent agent is a coding agent with
  `bash`; it must resolve conflicts and run its own verification. The runtime cannot
  do this on its behalf, and the user chose parent-driven merge.
- **A natural-language claim is not verifiable.** Trusting "I merged it" would let an
  LLM assert a fact the runtime never checks, defeating the `IntegrationValidation`
  guard that `docs/superpowers/plans/2026-08-10-reviewed-integration-boundaries.md`
  establishes.
- **State transitions are runtime-owned.** `can_transition_to`
  (`crates/yi-agent-core/src/subagent/task.rs:170`) protects the state machine; an
  agent must not drive it.
- **Deletion is irreversible.** Prompt-driven deletion could remove a worktree before
  its work is integrated, losing the deliverable with nothing to stop it.

The prompt's failure mode is safe: if the parent forgets to merge, the child stays in
`awaiting_parent_review` and is never recycled. The failure direction is "cleans up
less", never "deletes work".

## Part A — Integration Wiring

### A1. Parent integration instruction (prompt)

The mailbox payload a parent receives on delivery is the full `DeliveryReport` JSON
(`repository.rs:1499`), which carries `commit` (the child's delivered head SHA),
`evidence`, `changed_files`, and `known_limitations`. It does not carry the branch name,
and it does not need to: the parent can merge by SHA.

The parent agent's system prompt must instruct it, on receiving a delivery notification,
to merge the delivered commit into its own worktree (`git merge --no-ff <commit>`),
resolve conflicts, run verification, and then finish its turn.

### A2. Runtime completion verification

When a parent task's worker reports completion, the runtime verifies each child delivery
awaiting that parent's review:

```
git merge-base --is-ancestor <child.commit> <parent HEAD>
```

A pass proves the parent actually integrated that delivery. The runtime then calls
`RuntimeCoordinator::accept_review(child, IntegrationValidation::passed(...))`
(`runtime.rs:1859`), which completes the child task.

The hook is `reconcile_worker_events` (`runtime.rs:2447`), in the branch that handles a
**parent task's** completion event. This is the concrete meaning of "declared in the
parent's report": the report is the trigger, and the ancestry check is the proof. The
runtime does not parse agent text.

**Trade-off (accepted):** the ancestry check proves the commit is in the parent's HEAD,
not that the parent ran tests. The `IntegrationValidation` reason records the ancestor
relationship only; parent test evidence is not part of the gate.

## Part B — Recycling

### B1. New trait method

Add to `AgentWorkspaceService` (`crates/yi-agent-core/src/subagent/worker.rs:406`):

```rust
fn cleanup_accepted(&self, owner: &WorkerWorkspace, child: &WorkerWorkspace)
    -> Result<(), WorkerError> { Ok(()) }
```

Default is a no-op, matching the existing `cleanup_prepared` style.

### B2. Implementation

`DaemonWorkspaceService` maps the child `WorkerWorkspace` to
`ChildWorktree { path, branch, parent_branch, base_commit }`
(`worktree.rs:40`) and calls `WorktreeService::remove_accepted_clean(owner.path, ...)`
(`worktree.rs:254`). That method re-runs `merge-base --is-ancestor` itself and returns
`ChildNotMerged` when the branch is not contained, giving a second, git-level guard on
top of A2.

### B3. Hook point

At the end of `accept_review` (`runtime.rs:1859`), after the supervisor and SQLite
writes are durable and after `release_resident_lease`. The workspace service is resolved
exactly as `prepare_task_workspace` does (`runtime.rs:1469-1475`): prefer
`application_root_workspace_services[session]`, else `workspace_service`.

### B4. Recycle outside locks

Git runs as a subprocess and is slow. Recycling must never run while holding the
supervisor lock or the repository mutex. `accept_review` already drops the supervisor
guard before releasing the lease; recycling is sequenced last.

### B5. Recycle failure must not fail the accept

Acceptance is already durable when recycling runs. A recycle failure (for example a
dirty child worktree, which `remove_clean` refuses) emits a runtime event and leaves the
worktree in place for later handling. It must not roll back a completed review.

### B6. Delete the workspace row

Add a repository method to delete a task's `task_workspaces` row, called only after git
recycling succeeds. Delete the **child's** row only: a parent's row is still needed by
`prepare_child` for its remaining siblings, so it survives until the parent itself is
accepted. Deleting the row also frees `UNIQUE(repository_root, branch)`, removing the
collision risk from `short()` truncation.

## Verification

Deterministic (always run, no API key):

- **Unit** (`crates/yi-agent/src/subagent_runtime.rs` tests): `cleanup_accepted` against a
  real temporary git repo removes the worktree path and the branch ref.
- **Coordinator** (`crates/yi-agent-store/tests/runtime_coordinator.rs`): with a recording
  workspace service, `cleanup_accepted` is called for an integrated delivery and is **not**
  called when the ancestry check fails.
- **Refusal**: `cleanup_accepted` on an unmerged branch returns `ChildNotMerged` and
  leaves the worktree intact (extends `crates/yi-agent-tools/tests/subagent_worktree.rs`
  coverage).

Real-provider gate (`#[ignore]`, not run in CI): the existing
`real_subagent_accepts_delivery_into_parent_history`
(`crates/yi-agent/tests/subagent_real_e2e.rs:421`) becomes a genuine end-to-end check of
this path — it already asserts the delivered file is in the parent worktree and that the
child head is an ancestor of the parent HEAD.

## Documentation

Update `docs/project-management/subagent-runtime.md` with the new capability and its
verifiable criteria in the same change, and reconcile `docs/bug-list.md` entries for
worktree/branch accumulation.
