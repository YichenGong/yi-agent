# Real Subagent Completion Recovery Design

## Goal

Safely finish the pending child-worktree commit recovery changes, synchronize the branch with the newest `main`, and use one real-provider delivery workflow as the explicit gate before expanding real-provider rework and concurrent-isolation coverage.

## Scope

This work covers the pending changes in:

- `yi-agent-rs/crates/yi-agent-tools/src/sandbox.rs`
- `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs`
- `yi-agent-rs/crates/yi-agent/tests/subagent_real_e2e.rs`

It does not implement real-provider rework or concurrent-child workflow tests unless the real committed-delivery workflow passes against a reachable, authorized provider.

## Synchronization Safety

Before changing implementation, preserve the existing uncommitted patch in a named stash. Fetch the configured remote, rebase `fix/real-subagent-completion` onto the current `main`, restore the stash, and resolve only conflicts in the pending recovery scope.

After restoration, compare the branch with the rebased `main` and retain only changes that are not already present upstream. Rebase rewrites local commit identifiers; no remote force-push is part of this work.

## Child Git Sandbox Access

A coding child executes tools inside its assigned worktree under the configured sandbox mode. The child tool registry must grant write access only to:

1. the assigned worktree;
2. the worktree-specific Git directory returned by `git rev-parse --git-dir`;
3. the repository's Git common directory returned by `git rev-parse --git-common-dir`.

On macOS workspace-write sandbox profiles, `/dev/null` is additionally writable because Git opens it during commit creation. This exception does not grant write access to any repository path outside the child worktree and its required Git metadata.

## Dirty Delivery Recovery

After a child agent turn, the trusted workspace service inspects delivery state. A valid coding delivery remains subject to the existing constraints: a clean worktree, expected branch, a new `HEAD` descended from the recorded base, and a reachable fixed delivery commit with verification evidence.

If inspection reports that the child worktree is dirty, the worker must not report a successful or empty delivery. Instead, once per worker run, it starts another provider turn with a direct instruction to inspect status, stage all intended changes, create a commit in its assigned worktree, and verify that `git status --porcelain` is empty.

The recovery attempt clears the prior assistant report and does not consume a provider-retry budget. A second dirty-delivery result, or any other invalid delivery result, follows the existing durable failure path with its worktree evidence. The runtime must never stash, reset, copy, force-remove, or otherwise mutate a user checkout outside the child worktree.

## Deterministic Verification

The deterministic suite must prove:

- the child sandbox permits Git index and commit metadata writes necessary for a child worktree;
- a simulated child that first writes a file without committing receives the recovery instruction;
- the simulated child can commit on the next turn and produces a clean, delivered result;
- invalid delivery state is not reported as successful completion.

Formatting and diff whitespace checks are required. The focused worker regression runs through the `yi-agent` binary test target because this package has no library target.

## Real-Provider Gate

After deterministic verification, run the ignored real committed-delivery workflow with the configured real-provider endpoint. The workflow must demonstrate all of the following:

1. a root delegates exactly one coding child through `yi-agent run --subagents`;
2. the child creates the specified file and commits it in its assigned worktree;
3. the local authorized harness accepts delivery only through `PreviewReview` followed by `ConfirmReview`;
4. the accepted child commit is an ancestor of the parent `HEAD`;
5. the specified file and exact marker are visible in the parent worktree.

Provider unreachability, authentication failure, or failure to follow the required tool workflow is reported with existing redacted root/task/event diagnostics. Such a result is a gate failure, not evidence that the code repair passed, and it does not authorize rework or concurrency implementation.

## Documentation and Completion State

Project tracking must distinguish deterministic completion recovery coverage from real-provider workflow verification. The existing broad `Real-LLM delivery/rework/concurrency E2E` item remains incomplete until all required real scenarios pass. If and only if the real committed-delivery gate passes, a follow-up design and plan may add real rework correction and concurrent isolated-child delivery tests.
