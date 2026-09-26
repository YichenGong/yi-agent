# Worktree Directory Reclaim Design

## Goal

Stop the unbounded growth of `.worktrees/` by reclaiming worktree **directories**
automatically, without ever discarding unmerged work and without breaking session
reattachment.

Two triggers:

1. **On TUI exit** (double Ctrl+C): reclaim the session's worktree directories.
2. **On a 7-day TTL sweep**: reclaim directories for tasks that have been idle.
3. **On demand** (`yi-agent daemon gc`): the manual surface for everything the
   automatic paths refuse to touch.

## Problem

Observed state: `.worktrees/` grows without bound and `feat/yi-agent-*` branches
accumulate. This repository currently has 3 orphaned `feat/yi-agent-*-root`
branches, all `merged_into_main=NO`, each `ahead=1`, with no worktree attached —
and one live example of the real cost: `.worktrees/feat/desktop-gui-phase2/desktop/src-tauri/target/`
holds thousands of build artifacts.

Three root causes:

1. **Recycling is unreachable on exit.** The only production recycle path is
   `accept_review` → `recycle_accepted_delivery`
   (`yi-agent-rs/crates/yi-agent-store/src/runtime.rs:2067,2076`), which fires only
   when a parent has already merged the child's delivered commit and only on a
   reconcile pass. Closing a session does nothing.
2. **Nothing ever proposes a worktree for cleanup.** The TTL sweep discussed in
   `docs/superpowers/specs/2026-09-26-subagent-worktree-recycling-design.md` is out
   of scope there and does not exist. `detach_application_root`
   (`runtime.rs:971-1003`) performs three durable writes and no git work at all.
3. **The directory is where the bytes are.** Build artifacts (`target/`) are
   gitignored, so `git status --porcelain` reports a clean worktree that nonetheless
   occupies hundreds of megabytes.

## Core Insight: Directory Is Cache, Branch Is Data

Verified empirically in a scratch repository:

- `git worktree remove` deletes the working directory and the `.git/worktrees/<name>`
  administrative file. It does **not** touch the branch ref. After removal,
  `git show <branch>:<file>` still returns the committed content.
- `git worktree remove --force` silently discards **uncommitted** changes (a file
  created but never committed becomes unrecoverable with no warning).
- `git branch -d` refuses to delete a branch that is not merged into the current
  `HEAD`; `git branch -D` does not.

Therefore the safe automatic action is **delete the directory, keep the branch and
the row**. The reclaimable bytes live in the directory; the branch ref pins the
commits and costs almost nothing.

## Reattachment Does Not Depend on the Directory

This was verified directly, because it determines whether reclaiming the root
worktree breaks the macOS app's ability to resume a session.

`attach_application_root` (`runtime.rs:702-736`) branches on whether a
`task_workspaces` row exists:

- **Row present**: `application_root_workspace_matches` is called. That function
  (`yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs:406-414`) runs
  `git rev-parse --show-toplevel` in the **requested** workspace (the project root)
  and compares the result to `recorded.repository_root`. It references
  `recorded.path` **zero times**. A deleted worktree directory therefore does not
  affect the check. If the attachment state is `detached`, the runtime also
  reattaches it (`runtime.rs:737-742`).
- **Row absent, git repository**: hard error
  (`runtime.rs:725-731`, `"application root workspace does not match its recorded repository"`).

Session state lives in `sessions`, `tasks`, `mailbox_messages`, and `events`. No
table holds a foreign key to `task_workspaces`, so deleting a row cascades nowhere.
The only action that forfeits reattachment is **deleting the row**.

**Consequence for the design: reattachment is preserved as long as the row and
branch survive. The macOS app can close, reclaim the directory, and later resume
the same session with its full task history intact.**

## Decisions

| Decision | Choice | Rationale |
| --- | --- | --- |
| What is reclaimable | Directories only (branch and row preserved) | Reattachment and full recovery stay intact |
| "Merged" is relative to | The worktree's `parent_branch` | Matches `remove_accepted_clean`; checked explicitly via `merge-base --is-ancestor` |
| Dirty worktrees | Never reclaimed automatically | `--force` discards uncommitted work silently |
| Root worktree | Reclaimed when clean and the session is detached | The root's `parent_branch` is `main`, which it rarely merges into |
| Reclaim timing | `detach` returns immediately; a background thread does the work | Quit latency must not scale with worktree count |
| TTL | 7 days | Conservative default; reclaim is cheap and reversible |
| Manual surface | `yi-agent daemon gc` | The only exit for unmerged or dirty worktrees |

## "Merged" Is Verifiable, Not Assumed

Two different mechanisms enforce the two conditions, and it matters which applies
where.

**Cleanliness is enforced by git at removal time.** `git worktree remove` (without
`--force`) refuses a worktree with modified or untracked files. Part C calls exactly
this, so no separate check is strictly required — though the spec still records one
so a refusal can be logged distinctly from a git failure.

**Mergedness must be checked explicitly by this design.** The tempting argument is
that `git branch -d` self-certifies:

```
$ git branch -d feat/child          # from the owner worktree, HEAD = feat/root
退出码=0  Deleted branch feat/child (was 2a9b838).

$ git branch -d feat/child2         # same setup, child2 NOT merged
退出码=1  error: The branch 'feat/child2' is not fully merged.
```

That is true, but **this design does not delete branches** (see Part C3), so
`branch -d` never runs and certifies nothing. The automatic path must therefore run
its own ancestry check — the same one `remove_accepted_clean` performs
(`worktree.rs:266-277`):

```
git merge-base --is-ancestor <branch> <parent_branch>
```

`git branch -d`'s self-certification is what protects the branch deletions that
`daemon gc` (Part E) performs, and only those.

Note the asymmetry this produces, which is intended:

- **Child worktrees**: `parent_branch` is the root's branch (`create_child` sets it
  from the parent worktree's current branch, `worktree.rs:115`). A child whose
  delivery was merged into the root worktree HEAD passes the ancestry check.
- **Root worktrees**: `parent_branch` is `main` (`create_root` sets it from the
  repository's current branch, `worktree.rs:72`), which a root branch rarely merges
  into. The root therefore qualifies via the *detached* clause instead, and the
  ancestry check is not applied to it.

## Part A — Reclaim On TUI Exit

### A1. Trigger

Double Ctrl+C already exists: the first press arms `pending_quit` and interrupts a
running turn, the second returns `KeyOutcome::Quit`
(`yi-agent-rs/crates/yi-agent/src/tui/app.rs:902-907`), which breaks the run loop
(`app.rs:531`). `main.rs` then detaches (`main.rs:1396-1402`).

No new keystroke handling is required. The work is added to the detach path.

### A2. Ordering constraint

At the moment of the second Ctrl+C the session is still **attached**; detach happens
afterwards in `main.rs`. The `detached` condition therefore cannot be evaluated in
the TUI. It becomes true inside the detach handler, which is where the reclaim is
seeded.

### A3. detach handler shape

`detach_application_root` (`runtime.rs:971-1003`) keeps its three durable writes and
gains one step: spawn a background reclaim thread and return immediately.

```
a. authorize_application_root
b. pause_foreground_task(root_task, "foreground TUI detached")
c. transition root -> "paused", event TaskPaused
d. UPDATE application_root_attachments SET state='detached'   # durable
e. spawn reclaim thread (owns a cloned Arc<RuntimeCoordinator>)
f. return Ok(()) -> IpcResponse::ApplicationRootDetached
```

The spawn is available because `handle_client` already runs inside `thread::spawn`
(`ipc.rs:680`) and `coordinator` is an `Arc` (`ipc.rs:606`).

**Do not do the git work inline in the handler.** The client
`send_request_with_version` (`ipc.rs:830-843`) sets no read timeout, so a slow
handler makes the TUI wait for the full duration rather than fail. The server's 1s
socket timeouts (`ipc.rs:903-904`) bound only `read()`/`write()` syscalls, and the
request frame is already fully read before the handler runs. Correctness is not at
risk; quit latency is.

### A4. The embedded daemon dies with the process

`Daemon`'s listener is a `thread::spawn` (`ipc.rs:656`), not a subprocess, and
`Daemon::Drop` only calls `stop_listener()` (`ipc.rs:799-806`) — deliberately, since
starting a second runtime in a destructor would panic. So in the embedded case the
reclaim thread can be killed mid-flight.

This is why every reclaim step must be idempotent and re-runnable (Part C).

## Part B — TTL Sweep (7 days)

### B1. Hook point

The daemon listener already has a per-minute tick used for schedules
(`ipc.rs:663-673`, `last_schedule_minute` + `evaluate_schedules`). The sweep joins
that tick; no new thread is required.

### B2. Candidate condition

```
task has a task_workspaces row
AND task state is terminal   OR   (task is the root AND its session is detached)
AND tasks.updated_at is older than 7 days
```

`tasks.updated_at` is maintained on every transition (`repository.rs:2233`; 19 such
writes in the file), so it is a reliable idle clock.

`AwaitingParentReview` is **not** terminal — `is_terminal`
(`yi-agent-core/src/subagent/task.rs:226-239`) lists nine variants and does not
include it. Un-integrated deliveries are therefore never swept. This is the desired
behaviour and needs no extra guard.

`Paused` is also not terminal, which is precisely why the detached-root clause is
needed: a detached root sits in `paused` forever. Its only outbound transitions are
`Queued`, `Cancelled`, and `RecoveryRequired` (`task.rs:208-209`), all user- or
worker-initiated, and a detached root has no worker handle
(`worker_task_ids()` returns `self.workers.keys()`, `supervisor.rs:397-399`), so the
automatic escape is unreachable too. Without this clause the root is never a
candidate.

### B3. New repository query

No existing query serves this. `task_snapshots` (`repository.rs:3356`) neither
filters by session nor carries `depth`; `application_root_hydration_tasks`
(`repository.rs:3596`) filters by session and orders by depth but does not join
`task_workspaces`. Add a query joining `tasks` + `task_workspaces` filtered by
`root_session_id` and ordered by `depth DESC`, so the deepest worktrees are handled
first. `PersistedTaskDetail` (`repository.rs:317-326`) is the right row shape.

## Part C — Reclaim Procedure

For each candidate, deepest first:

```
1. Acquire the session's supervisor lock
2. Re-verify candidacy inside the lock          # closes the TOCTOU window
3. git status --porcelain in the worktree       # non-empty -> skip
4. Ancestry check (children only, not root):
     git merge-base --is-ancestor <branch> <parent_branch>
                                                 # non-zero -> skip
5. Reclaim directory only:
     git worktree remove <path>                 # no --force
6. Release the supervisor lock
7. Append TaskWorkspaceRecycled (existing event, repository.rs:100)
```

Steps 3 and 4 are belt-and-braces: git enforces both at step 5 for the dirty case
and would enforce mergedness at `branch -d` time, which this design does not reach.
Running them explicitly means a refusal is attributable — a dirty worktree and an
unmerged branch produce different log lines and different `daemon gc` listings
rather than one opaque git error.

Step 4 is skipped for the root worktree: the root qualifies via the detached clause,
and its `parent_branch` is `main`, against which its branch is normally unmerged by
design.

### C1. Why deepest first

`remove_accepted_clean` runs its ancestry check with `cwd = owner_worktree`
(`worktree.rs:254-286`). Verified: removing a parent worktree first makes a child's
check fail with `exit 128: cannot change to '.worktrees/root': No such file or
directory`. Ordering Leaf(2) → Child(1) → Root(0) avoids this.

### C2. Lock discipline

Git runs as a synchronous subprocess (10 `Command::new("git")` calls in
`worktree.rs`; zero `spawn_blocking` in the workspace). The existing lock order is
**supervisor → repository**: `start_worker` takes the supervisor guard
(`runtime.rs:1235-1236`) and then calls `prepare_task_workspace`, which locks the
repository (`runtime.rs:1615-1623`).

The reclaim must follow the same order and must not run git while holding the
repository lock:

```
1. repository lock -> read candidates -> release      (short, no git)
2. per candidate: supervisor lock
                  repository lock -> re-verify -> release   (short, no git)
                  run git                                   (supervisor lock only)
                  release supervisor lock
```

Holding the supervisor lock across the git call is deliberate. The competing actor
is `retry_task`, which mutates state under the same lock (`runtime.rs:1685-1688`).
Serializing them means only two orderings are possible, both correct:

- reclaim first: directory removed, then retry rebuilds it via the Part D path
- retry first: task leaves the terminal set, so the re-verification in step 2 skips it

Releasing the lock would reintroduce a TOCTOU window whose worst outcome is a worker
starting in a directory that was just deleted.

This project has a real precedent for lock-order bugs: `release_resident_lease`
(`runtime.rs:251-262`) carries a comment documenting a `resource_coordinator ->
resident_*` inversion that deadlocked the reconcile loop. Follow the documented order
rather than inventing one.

### C3. Idempotence

| Step | Behaviour when re-run |
| --- | --- |
| `git status --porcelain` | Read-only; always safe |
| `git merge-base --is-ancestor` | Read-only; always safe |
| `git worktree remove <path>` | Errors on an already-absent path; treat as done |
| `git branch -d <branch>` | Not called by this design |
| row deletion | Not called by this design |

Since Part A1 deletes neither the branch nor the row, the interrupted state is at
worst "directory gone, branch and row present" — which is exactly the state Part D
must handle anyway.

## Part D — Rebuild Path (Required)

`prepare_task_workspace` is row-idempotent (`runtime.rs:1615-1623`): a present row is
reused and returned without checking that `path` exists. After a reclaim, the next
`start_worker` for that task would therefore hand a worker a non-existent directory.
Verified: `git -C <removed path> status` fails with `exit 128`.

Add an existence check at the row-hit branch:

```
row present -> workspace.path exists?
                 yes -> reuse (current behaviour)
                 no  -> service.reattach_workspace(&workspace)
```

`reattach_workspace` must attach the **existing** branch:

```
git worktree add <path> <branch>
```

Not the existing `add_worktree` helper (`worktree.rs:462-467`), which always passes
`-b` and therefore fails with `fatal: a branch named '<branch>' already exists` when
the branch survives. Both forms were verified against a scratch repository: the
attach form exits 0 and restores the branch tip exactly, with committed content
intact.

This is what makes Part A1's "keep the branch" decision pay off: rebuild is a single
command because the branch is still there.

## Part E — `yi-agent daemon gc`

The only surface that can act on worktrees the automatic paths refuse: dirty
worktrees, unmerged branches, and orphaned rows.

Add a `Gc` variant to `DaemonAction` (`yi-agent-rs/crates/yi-agent/src/config.rs:256-266`,
currently `Start` / `Status` / `Stop` / `Serve`).

`gc` lists every reclaimable-but-not-reclaimed object with enough context to decide:

- task id, branch, worktree path, state, age
- merged into `parent_branch`: yes/no
- dirty: yes/no

Destructive actions require explicit confirmation, reusing the existing
`PreviewCancel` / `ConfirmCancel` token pattern (`ipc.rs:188-196` plus
`ConfirmationStore`): a one-shot token with a TTL, validated against the task id and
scope on consume.

Scope of what `gc` may delete:

- directories (same as automatic paths, but including dirty ones with confirmation)
- branch refs (`git branch -D`, explicitly confirmed)
- `task_workspaces` rows — **this is the only operation that forfeits reattachment**,
  and it must be labelled as such in the confirmation prompt

## Non-Goals

- Reclaiming worktrees for tasks that are still running or awaiting review.
- Merging anything. This design never creates a merge commit.
- Changing `detach_application_root`'s three durable writes.
- Deleting the root worktree's branch on exit.

## Verification

Deterministic, no API key:

**Reclaim primitive** (`yi-agent-rs/crates/yi-agent-tools/tests/subagent_worktree.rs`)

- `remove_clean` on a clean worktree removes the directory and leaves the branch ref
  and its commits reachable.
- `remove_clean` on a dirty worktree returns `DirtyChild` and leaves everything in
  place.
- `merge-base --is-ancestor` reports an unmerged child branch as not-an-ancestor, so
  Part C step 4 refuses it.

**Rebuild path** (`crates/yi-agent/src/subagent_runtime.rs` tests)

- After a directory reclaim, `reattach_workspace` restores the worktree at the
  recorded path with the branch tip equal to the pre-reclaim tip.
- Attaching with `-b` fails on a surviving branch (guards against regressing to the
  `add_worktree` helper).

**Ordering** (`crates/yi-agent-store/tests/runtime_coordinator.rs`)

- With a parent and child worktree, reclaiming deepest-first succeeds; reclaiming the
  parent first produces the `exit 128` failure (documents why order matters).

**Detach seeding** (`crates/yi-agent-store/tests/runtime_ipc.rs`)

- `DetachApplicationRoot` returns promptly and leaves the attachment `detached`.
- The reclaim thread is spawned (observable via a recording workspace service).
- An interrupted reclaim (no directory removal, no branch deletion, row intact)
  leaves a state that the next `prepare_task_workspace` repairs via the Part D path.

**TTL candidacy** (`crates/yi-agent-store/tests/runtime_coordinator.rs`)

- A detached root in `paused` past the TTL is a candidate (this is the clause that
  makes root reclaimable at all).
- A child in `awaiting_parent_review` is never a candidate, regardless of age.

**Reattachment preserved** (`crates/yi-agent-store/tests/runtime_ipc.rs`)

- Detach, reclaim the root directory, then reattach with the same idempotency key:
  the attachment succeeds and the task history is intact. This is the regression
  guard for the macOS resume requirement.

## Documentation

Update `docs/project-management/subagent-runtime.md` with the new capability and its
verification command in the same change, and reconcile the
`docs/bug-list.md` entry for worktree accumulation, noting that exit-time and TTL
reclaim land while dirty/unmerged worktrees remain a `daemon gc` concern.
