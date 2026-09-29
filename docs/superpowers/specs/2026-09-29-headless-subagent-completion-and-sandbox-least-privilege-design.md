# Headless Subagent Completion and Sandbox Least Privilege Design

## Goal

Close the two remaining blockers found while rebasing `fix/real-subagent-completion` onto current `main`, so the real-provider delivery gate can pass end to end:

1. A coding child can escalate write access inside the repository Git common directory beyond its own worktree.
2. A headless root process exits as soon as its own turn ends, so it never integrates an accepted child delivery.

## Scope

Modify:

- `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs` — narrow the child Git writable-root set; raise the `wait_agent` wait budget.
- `yi-agent-rs/crates/yi-agent/src/main.rs` — keep the headless root alive while non-terminal children exist, then let it integrate.
- `yi-agent-rs/crates/yi-agent/tests/subagent_real_e2e.rs` — raise harness waits that would otherwise pre-empt the new budget.

Do not implement real rework or concurrent-child scenario coverage in this work.

## Already-committed prerequisites

Two defects on this branch are already fixed and verified. They are prerequisites, not open work:

- Child Git commits were impossible under the deny-by-default sandbox profile (`/dev/null` and the shared Git common directory were not writable).
- A dirty child delivery failed durably instead of receiving one commit instruction.

## Part 1: Child Git sandbox least privilege

### Defect

`git_writable_roots_for_worktree` returns the whole `git rev-parse --git-common-dir` (`$REPO/.git`). Measured on a real repository with two child worktrees, a child under the resulting profile could write:

| Target | Current (whole `.git`) | Required |
| --- | --- | --- |
| `.git/config` (remap remotes) | WRITABLE | denied |
| `.git/hooks/pre-commit` (plant a hook) | WRITABLE | denied |
| sibling worktree `index` | WRITABLE | denied |
| sibling worktree `HEAD` | WRITABLE | denied |
| own `objects` / `refs` / `logs` | WRITABLE | required |

The sibling-worktree writes are the serious case: one coding child can forge or repoint another child's index and HEAD.

### Change

Replace the single common-directory root with the minimal set that still commits. Measured minimal set, same repository and same child:

- `$REPO/.git/objects`
- `$REPO/.git/refs`
- `$REPO/.git/logs`

Verified: with this set `git add` plus `git commit` still succeeds and the worktree ends clean; `.git/config`, `.git/hooks`, and sibling worktrees are all denied. Omitting `logs` fails with `unable to append to '.git/logs/refs/heads/<branch>'`, so `logs` is required, not optional.

### Regression coverage

No existing test asserts these boundaries. Add denials for `.git/config`, `.git/hooks`, and a sibling worktree `index`/`HEAD`, alongside the existing proof that staging and commit succeed.

## Part 2: Headless root completion

### Defect

The headless root registers `wait_agent`, and the default system prompt already instructs the parent to integrate deliveries with `git merge --no-ff`. The failure is that `run_headless` drains the agent stream and immediately calls `std::process::exit`, and the agent loop `return`s on `Done`. The parent therefore exits while the child is still awaiting review, so no later wake-up can reach it.

`AwaitingParentReview` is not terminal (`TaskState::is_terminal` excludes it), so "children still non-terminal" is exactly the window covering mandatory human review.

### Change

After the root's own stream completes and before detaching, add a bounded completion step in the headless path:

- While the session has non-terminal direct children, issue an authorized `WaitAgent` with `mode = all` and a 600-second timeout.
- On every return (completed or timeout), re-check for non-terminal children. This is the loop: a timed-out wait is followed by a new one.
- Stop when all children are terminal, or when a total wait budget is exhausted.
- On exhaustion, report explicitly that review is still pending and exit without discarding work.

### Wait budget and interruptibility

`DaemonWaitAgentTool` reads one shared constant, `TUI_WAIT_AGENT_TIMEOUT_MS`, used by both TUI and headless registrations.

Decision: raise the per-call wait to 600 seconds for both paths, because subagent work is long-running and one constant serves both. This follows existing precedent: the value was already deliberately raised from 30 s to 120 s for the same reason.

Accepted cost, measured rather than assumed: `DaemonWaitAgentTool::call` performs a synchronous `send_request` (no `spawn_blocking`), and tool futures are awaited in the same task as the agent loop via `join_all`, so cancellation cannot pre-empt the block. A probe test confirmed a non-yielding blocking tool delays cancellation to the full block duration (2.006 s observed for a 2 s block), while a yielding tool cancels immediately (0.10 s). Raising the value to 600 s therefore extends the non-interruptible window from 2 minutes to 10 minutes: a TUI user pressing Esc may wait up to 10 minutes for the interface to respond. This is accepted deliberately.

Total budget: 30 minutes, expressed as a named constant with a small test-mode override so CI does not wait. On exhaustion the process reports pending review and exits; the work is not lost.

### Why work is not lost

Repository policy already guarantees it: delivered-but-unintegrated branches and their `task_workspaces` rows are deliberately retained, and a reclaimed directory is rebuilt from the branch on next worker start. A timeout can therefore never discard an accepted or pending delivery.

## Containment of test harness waits

Two harness waits are 300 seconds and would pre-empt the new 600-second budget:

- `await_direct_child` polls until the root spawns a child, and panics if the root exits early.
- `await_review` polls until the child reaches `awaiting_parent_review`.

Both must be raised above the new budget so the harness observes the intended behavior instead of failing first.

## Deterministic Verification

Required before any real-provider run:

- Sandbox: staging and commit succeed under the minimal root set; `.git/config`, `.git/hooks`, and sibling worktree `index`/`HEAD` writes are denied.
- Headless completion: with a non-terminal child the root does not exit; once children are terminal it exits and integrates.
- Budget: exhaustion reports pending review rather than hanging or silently succeeding.
- Formatting and diff whitespace checks pass.

## Real-Provider Gate

After deterministic verification, run the ignored committed-delivery workflow against the configured endpoint. It must show: one coding child spawned, the child committing the marker file, acceptance through `PreviewReview` then `ConfirmReview`, the accepted commit an ancestor of the parent `HEAD`, and the marker visible in the parent worktree.

Provider unreachability, authentication failure, or refusal to follow the tool workflow is a gate failure, not evidence the repair passed, and does not authorize rework or concurrency work.

## Documentation and Completion State

The tracking row `Real-LLM delivery/rework/concurrency E2E` stays unchecked until all required real scenarios pass. Sandbox least privilege and headless completion are recorded as their own verified items.

## Assumptions Requiring Confirmation

1. The 600-second value applies to TUI as well as headless, accepting the measured 10-minute non-interruptible window in the interactive UI.
2. The total completion budget is 30 minutes.
3. Harness waits are raised above 600 seconds.
