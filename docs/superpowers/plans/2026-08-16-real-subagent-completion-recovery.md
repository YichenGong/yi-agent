# Real Subagent Completion Recovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rebase the pending real-subagent completion repair onto current `main`, preserve child-worktree Git sandbox isolation, recover one dirty coding delivery by requesting a commit, and use the real committed-delivery workflow as the gate for later real rework/concurrency work.

**Architecture:** Keep delivery validity in `DaemonWorkspaceService` and make the worker react to its existing dirty-worktree error once. Construct each child tool registry from the assigned worktree plus its Git metadata roots so sandboxed Git can stage and commit without making unrelated repository paths writable. The ignored real-provider test remains an external gate: it proves the complete parent/child/IPC acceptance flow but does not authorize new real scenarios on provider failure.

**Tech Stack:** Rust 2024 workspace, Tokio, Git worktrees, macOS `sandbox-exec` / Linux Bubblewrap sandbox backends, SQLite daemon IPC, Cargo, Just.

## Global Constraints

- Preserve the uncommitted patch in a named stash before fetch/rebase; do not discard, reset, or force-clean it.
- Rebase only `fix/real-subagent-completion` onto the fetched current `main`; do not force-push any branch.
- Resolve conflicts only in `sandbox.rs`, `subagent_runtime.rs`, `subagent_real_e2e.rs`, or their direct documentation.
- Child writes remain limited to its assigned worktree, its worktree-specific Git directory, and its Git common directory; `/dev/null` is the sole macOS device exception.
- A dirty child delivery receives exactly one recovery instruction; subsequent invalid delivery state must remain a durable failure, never a successful or empty delivery.
- Real-network tests run only after deterministic tests pass and only through explicit ignored-test invocation or `just test-real-subagent`.
- Do not mark the combined delivery/rework/concurrency tracking row complete unless all four real scenarios pass.

---

## File Structure

- Modify: `yi-agent-rs/crates/yi-agent-tools/src/sandbox.rs` — macOS profile exception needed by Git commit while retaining deny-by-default writes.
- Modify: `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs` — child Git writable-root discovery, one-shot dirty-delivery recovery, and deterministic worker coverage.
- Modify: `yi-agent-rs/crates/yi-agent/tests/subagent_real_e2e.rs` — accurate delivery JSON extraction and redacted diagnostics for terminal child states/failed parent acceptance.
- Modify: `docs/project-management/subagent-runtime.md` — record only evidence actually obtained after deterministic and real-provider gates.
- Create: `docs/superpowers/specs/2026-08-16-real-subagent-completion-recovery-design.md` — approved behavior specification; already committed as `fb9dc04`.
- Create: `docs/superpowers/plans/2026-08-16-real-subagent-completion-recovery.md` — this execution plan.

## Task 1: Preserve the patch and synchronize with current main

**Files:**
- Modify: Git index/history only; no source edit is expected before stash restoration.

**Interfaces:**
- Consumes: branch `fix/real-subagent-completion`; remote `origin`; local branch `main`.
- Produces: the same pending source patch restored atop the fetched `origin/main` commit and an auditable `git range-diff`/`git diff` against rebased `main`.

- [ ] **Step 1: Record the starting state and stash only the implementation patch**

Run from the linked worktree:

```bash
git status --short --branch
git diff --check
git stash push -m 'wip: real subagent completion recovery before main rebase' -- \
  yi-agent-rs/crates/yi-agent-tools/src/sandbox.rs \
  yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs \
  yi-agent-rs/crates/yi-agent/tests/subagent_real_e2e.rs
git stash list -1
git status --short --branch
```

Expected: the named stash is `stash@{0}` and the worktree is clean because the specification commit is already recorded.

- [ ] **Step 2: Fetch and identify the exact upstream main commit**

```bash
git fetch origin main
git show -s --format='origin/main=%H%n%an%n%ad%n%s' --date=iso-strict origin/main
git log --oneline --decorate --graph -20 --all
```

Expected: `origin/main` resolves successfully and its SHA is captured in the task notes before rebase.

- [ ] **Step 3: Rebase the local branch and restore the named stash**

```bash
git rebase origin/main
git stash pop stash@{0}
```

If rebase conflicts, inspect only the files named in Global Constraints, resolve with the current `main` behavior preserved plus this feature’s changes, then run:

```bash
git add <resolved-files>
git rebase --continue
git stash pop stash@{0}
```

If stash restoration conflicts, resolve the same scoped files, `git add` them, and do not create a commit yet.

- [ ] **Step 4: Inspect the restored delta and remove any code already supplied by main**

```bash
git diff --check origin/main...HEAD
git diff --check
git diff --name-status origin/main...HEAD
git diff -- yi-agent-rs/crates/yi-agent-tools/src/sandbox.rs \
  yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs \
  yi-agent-rs/crates/yi-agent/tests/subagent_real_e2e.rs
```

Expected: the remaining uncommitted patch contains only the intended sandbox, recovery, and diagnostics changes; no duplicate upstream implementation remains.

- [ ] **Step 5: Commit the rebased specification if rebase made it uncommitted**

```bash
git status --short
git log -1 --oneline
```

Expected: the specification commit is retained in history. Do not create an empty synchronization commit; source changes remain uncommitted for the test-first tasks below.

## Task 2: Prove child Git metadata access remains sandbox-scoped

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs:worker_tool_registry`, Git-root helper, and its `mod tests` section.
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/sandbox.rs:platform_command`.

**Interfaces:**
- Consumes: `WorkerWorkspace { path: PathBuf, .. }`, `git_output(workspace, ["rev-parse", "--git-dir"])`, `git_output(workspace, ["rev-parse", "--git-common-dir"])`.
- Produces: `git_writable_roots_for_worktree(workspace: &Path) -> Vec<PathBuf>` and a tool registry whose sandbox writable roots are `[workspace.path, git_dir, git_common_dir]`.

- [ ] **Step 1: Add or tighten the failing sandbox test before implementation**

In `subagent_runtime.rs` adjacent to `child_tool_registry_enforces_workspace_sandbox_boundaries`, make the worktree-backed test invoke both staging and commit:

```rust
let bash = registry.get("bash").expect("bash tool");
assert!(
    !bash
        .call(json!({
            "command": "git add delivery.txt && git -c user.name=test -c user.email=test@example.invalid commit -m delivery"
        }))
        .await
        .is_error,
    "child sandbox must permit Git index and commit metadata writes",
);
```

Also assert the worktree is clean and `git rev-parse HEAD` differs from its base after the command. Keep the existing assertion that writing outside the child workspace fails.

- [ ] **Step 2: Run the focused sandbox test to establish the pre-fix failure when applicable**

```bash
cd yi-agent-rs
cargo test -p yi-agent --bin yi-agent \
  subagent_runtime::tests::child_tool_registry_enforces_workspace_sandbox_boundaries -- --exact
```

Expected before the metadata-root implementation: a sandbox-denied Git index, lock, ref, or commit write. If current `main` already contains the implementation after Task 1, record that this test is already green and continue without changing behavior.

- [ ] **Step 3: Implement the minimal child Git writable-root set**

In `worker_tool_registry`, assemble roots exactly as follows:

```rust
let mut writable_roots = vec![workspace.path.clone()];
writable_roots.extend(git_writable_roots_for_worktree(&workspace.path));
register_builtin_tools_with_sandbox(
    &mut tools,
    workspace.path.clone(),
    self.sandbox,
    writable_roots,
);
```

Implement `git_writable_roots_for_worktree` by using existing `git_dir_for_worktree`, then resolving `git rev-parse --git-common-dir`. Convert a relative common directory to `workspace.join(common_dir)`; preserve absolute paths. Return no Git roots if no Git directory can be resolved.

In the macOS workspace-write profile, append:

```rust
policy.push_str("(allow file-write* (literal \"/dev/null\"))\n");
```

immediately after the deny-root policy and before iterating writable roots. Keep the explanatory comment and do not add any broader device or root permission.

- [ ] **Step 4: Run focused test and boundary regression**

```bash
cargo test -p yi-agent --bin yi-agent \
  subagent_runtime::tests::child_tool_registry_enforces_workspace_sandbox_boundaries -- --exact
cargo test -p yi-agent --bin yi-agent \
  subagent_runtime::tests::child_tool_registry_allows_git_index_writes -- --exact
```

Expected: both tests pass, confirming child Git writes work while outside-workspace writes remain denied. If the exact existing index test has a different registered module path, obtain it first with:

```bash
cargo test -p yi-agent --bin yi-agent -- --list | grep -E 'child_tool_registry.*git|git_index'
```

and run its complete registered name with `--exact`.

- [ ] **Step 5: Commit the sandbox access repair**

```bash
cd ..
git add yi-agent-rs/crates/yi-agent-tools/src/sandbox.rs yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs
git commit -m 'fix: permit child Git commit metadata in sandbox'
```

Expected: the commit contains only sandbox policy/root-discovery code and the associated deterministic test changes.

## Task 3: Recover exactly one dirty delivery by requesting a commit

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs:AgentWorkerFactory::start`, helper functions, fake provider, and worker tests.

**Interfaces:**
- Consumes: `DaemonWorkspaceService::inspect_delivery(&WorkerWorkspace) -> Result<DeliveryReport, WorkerError>`.
- Produces: `is_dirty_delivery_error(error: &WorkerError) -> bool`; a single recovery provider turn using the explicit commit instruction; normal `WorkerEvent::Delivered(DeliveryReport)` after valid repair.

- [ ] **Step 1: Write the failing recovery test with a deterministic provider sequence**

Add `DirtyDeliveryProvider` in the existing test module. Its sequence must be:

1. first call uses `bash` to write `delivery.txt` and ends turn;
2. second call ends turn so runtime inspects the dirty worktree;
3. third call uses `bash` to run `git add delivery.txt && git commit -m 'deliver'` and ends turn;
4. fourth and fifth calls end turn so the agent loop and delivery inspection complete.

Add `daemon_worker_requests_a_commit_for_a_dirty_delivery_before_reporting_failure` that creates a root worktree, starts a `DaemonAgentWorkerFactory`, polls `handle.take_events()`, and fails immediately on `WorkerEvent::Failed`. Assert:

```rust
assert_eq!(delivery.workspace, workspace.lease_id);
assert_eq!(git_output(&root.path, &["status", "--porcelain"]), None);
assert_eq!(requests.len(), 4);
assert!(requests[2].messages.iter().any(|message| {
    message.content.to_string().contains("git commit -m")
}));
```

Use the existing two-second timeout/polling pattern.

- [ ] **Step 2: Run the exact test to verify it fails before recovery logic**

```bash
cd yi-agent-rs
cargo test -p yi-agent --bin yi-agent \
  subagent_runtime::tests::daemon_worker_requests_a_commit_for_a_dirty_delivery_before_reporting_failure \
  -- --exact
```

Expected before implementation: failure at the first dirty-worktree delivery inspection, before a commit-provider turn occurs.

- [ ] **Step 3: Implement the single recovery branch**

In the worker thread’s provider loop, declare before `'run: loop`:

```rust
let mut requested_delivery_commit = false;
```

At the `inspect_delivery` match, before empty-delivery fallback, handle the dirty error:

```rust
Err(error) if !requested_delivery_commit && is_dirty_delivery_error(&error) => {
    requested_delivery_commit = true;
    prompt = "Your worktree contains uncommitted changes, so the delivery cannot be reviewed. Run `git status --porcelain`, then stage every intended change with `git add` and create a commit with `git commit -m` in your assigned worktree. Do not only describe the commands; execute them. After committing, verify `git status --porcelain` is empty.".into();
    retrying_provider = false;
    assistant_report.clear();
    continue 'run;
}
```

Add:

```rust
fn is_dirty_delivery_error(error: &WorkerError) -> bool {
    error.to_string().contains("child worktree is dirty")
}
```

Do not alter the existing `is_empty_delivery_error` branch. The Boolean prevents a second recovery prompt and preserves the normal failure behavior after another invalid inspection.

- [ ] **Step 4: Run focused recovery and adjacent delivery tests**

```bash
cargo test -p yi-agent --bin yi-agent \
  subagent_runtime::tests::daemon_worker_requests_a_commit_for_a_dirty_delivery_before_reporting_failure \
  -- --exact
cargo test -p yi-agent --bin yi-agent \
  subagent_runtime::tests::daemon_worker_reports_a_structured_delivery_when_it_completes_cleanly \
  -- --exact
cargo test -p yi-agent --bin yi-agent \
  subagent_runtime::tests::daemon_worker_reports_text_completion_without_a_workspace_delivery \
  -- --exact
```

Expected: each command reports `1 passed; 0 failed`.

- [ ] **Step 5: Commit recovery behavior and its regression test**

```bash
cd ..
git add yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs
git commit -m 'fix: request commit for dirty subagent delivery'
```

Expected: the commit contains only dirty-delivery detection, one-shot recovery, and deterministic provider-based coverage.

## Task 4: Make the ignored real delivery test report correct evidence

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/tests/subagent_real_e2e.rs:await_review` and `real_subagent_accepts_delivery_into_parent_history`.

**Interfaces:**
- Consumes: `IpcTaskDetail.delivery_json`, `IpcRequest::ReadTaskEvents`, `IpcResponse::TaskEvents`, root process output, and Git commands scoped to the temporary fixture.
- Produces: real-test diagnostics that expose terminal state, terminal evidence, child worktree status/log, task events, child commit/tree, and parent worktree state without printing credentials.

- [ ] **Step 1: Add assertions for the delivery report’s canonical commit field**

In `real_subagent_accepts_delivery_into_parent_history`, parse `delivery.delivery_json` and read:

```rust
let child_head = delivery_json["commit"]
    .as_str()
    .unwrap_or_else(|| panic!("delivery commit missing from {delivery_json}"))
    .to_owned();
```

Do not use the obsolete `head_commit` key. Before accepting, preserve the existing preview/confirmation flow. After acceptance, read `parent_workspace.join(DELIVERY_FILE)`, assert exact marker content, and retain `git merge-base --is-ancestor <child_head> HEAD`.

- [ ] **Step 2: Improve terminal-state diagnostics without changing test pass criteria**

In `await_review`, treat `failed`, `completed_no_changes`, and `cancelled` as terminal failures before review. On those states call `ReadTaskEvents`, format only its debug event data, and panic with task state, `terminal_json`, workspace status/log, and events.

When the accepted parent file cannot be read, collect only temporary-fixture Git evidence:

```rust
let parent_status = git(&parent_workspace, &["status", "--porcelain"]);
let parent_log = git(&parent_workspace, &["log", "--oneline", "-3"]);
let child_tree = git(&parent_workspace, &["show", "--format=", "--name-only", &child_head]);
```

Panic with those values and `delivery_json`. Do not print process environment, command environment, or API keys.

- [ ] **Step 3: Run compile-only and default network-free test checks**

```bash
cd yi-agent-rs
cargo test -p yi-agent --test subagent_real_e2e --no-run
cargo test -p yi-agent --test subagent_real_e2e -- --skip real_subagent_accepts_delivery_into_parent_history --skip real_subagent_configuration_skips_without_keys_or_runs_without_leaking_key
```

Expected: compilation succeeds; default test invocation does not execute ignored real-provider tests. If zero non-ignored tests are registered in this test target, Cargo reports `0 passed; 0 failed`, which is acceptable evidence that ignored tests were not run.

- [ ] **Step 4: Commit real-delivery evidence improvements**

```bash
cd ..
git add yi-agent-rs/crates/yi-agent/tests/subagent_real_e2e.rs
git commit -m 'test: improve real subagent delivery diagnostics'
```

Expected: this commit changes delivery JSON extraction and diagnostics only; it does not add rework or concurrency scenarios.

## Task 5: Run the deterministic quality gate and real-provider delivery gate

**Files:**
- Modify: `docs/project-management/subagent-runtime.md` only after command evidence is known.

**Interfaces:**
- Consumes: committed Tasks 2–4, configured endpoint environment loaded only by `just test-real-subagent`, and the ignored test `real_subagent_accepts_delivery_into_parent_history`.
- Produces: recorded outcome: either real committed-delivery gate passed, or an explicit provider/workflow gate failure with tracking row still incomplete.

- [ ] **Step 1: Run static and deterministic verification from the rebased branch**

```bash
cd yi-agent-rs
cargo fmt --check
cd ..
git diff --check origin/main...HEAD
cd yi-agent-rs
cargo test -p yi-agent --bin yi-agent \
  subagent_runtime::tests::child_tool_registry_enforces_workspace_sandbox_boundaries -- --exact
cargo test -p yi-agent --bin yi-agent \
  subagent_runtime::tests::daemon_worker_requests_a_commit_for_a_dirty_delivery_before_reporting_failure -- --exact
cargo test -p yi-agent --bin yi-agent \
  subagent_runtime::tests::daemon_worker_reports_a_structured_delivery_when_it_completes_cleanly -- --exact
```

Expected: every command exits zero. Capture warnings separately; warnings do not convert a passing test result into a failure unless the command itself treats them as errors.

- [ ] **Step 2: Run exactly the ignored committed-delivery gate**

From `yi-agent-rs`, explicitly load only the documented test recipe and invoke its focused test:

```bash
if [ -f "$HOME/.yi-agent/.env" ]; then set -a; . "$HOME/.yi-agent/.env"; set +a; fi
if [ -f .env ]; then set -a; . ./.env; set +a; fi
cargo test -p yi-agent --test subagent_real_e2e \
  real_subagent_accepts_delivery_into_parent_history -- --ignored --exact --test-threads=1
```

Expected gate pass: the command reports `1 passed; 0 failed`, and the test’s own structural assertions prove commit, two-step acceptance, ancestry, and parent-file visibility.

- [ ] **Step 3: Apply the C-phase decision rule from observed output**

If Step 2 passes, record that the real committed-delivery gate passed and stop this plan after documentation: rework/concurrency require their own approved follow-up spec and plan.

If Step 2 fails because provider networking, authentication, or model tool behavior prevents the workflow, retain all deterministic commits, do not mark real E2E complete, do not add rework/concurrency tests, and record the redacted failure category and command in project tracking.

If Step 2 fails at a deterministic application assertion, investigate that regression with `systematic-debugging` before editing implementation; do not classify it as a provider failure without evidence.

- [ ] **Step 4: Update tracking with only verified facts**

In `docs/project-management/subagent-runtime.md`, keep this row unchecked in every C-phase outcome:

```markdown
- [ ] Real-LLM delivery/rework/concurrency E2E — ...
```

Append evidence to the row’s text:

- on gate pass: state that committed-delivery acceptance passed with the exact focused command and date, while rework/concurrency remain pending;
- on gate failure: state the redacted failure class, the exact focused command, and that delivery/rework/concurrency remains incomplete.

Do not include API keys, endpoint query parameters, raw authorization headers, or unredacted provider payloads.

- [ ] **Step 5: Commit the verified tracking update and run final checks**

```bash
git add docs/project-management/subagent-runtime.md
git commit -m 'docs: record real subagent delivery gate'
cd yi-agent-rs
cargo fmt --check
cd ..
git diff --check origin/main...HEAD
git status --short --branch
git log --oneline origin/main..HEAD
```

Expected: formatting and diff checks exit zero; status is clean; history contains the specification, sandbox, recovery, diagnostics, and evidence-based tracking commits.

## Required Execution Checkpoints

1. Do not rebase until the named stash appears in `git stash list`.
2. Do not run real-provider tests until Task 5 Step 1 passes.
3. Do not implement rework or concurrent-isolation scenarios under this plan, including after a successful delivery gate; write and approve a follow-up design and plan first.
4. Do not claim provider success without the focused ignored test reporting `1 passed; 0 failed`.
5. Before reporting completion, include the final `cargo fmt --check`, `git diff --check origin/main...HEAD`, focused deterministic tests, exact real-gate result, and clean `git status` output.
