# Real Subagent Workflow Regressions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extend the ignored real-provider subagent suite with structural delivery acceptance, rework, and concurrent-worktree-isolation regressions.

**Architecture:** Keep all new behavior in `crates/yi-agent/tests/subagent_real_e2e.rs`. Each test creates an independent `TempDir`, initializes a disposable Git repository with one baseline commit, invokes the current worktree's `yi-agent` binary with the dedicated real-test configuration, and inspects only files and Git objects in that temporary repository. Reuse the existing write-only resolver and 300-second owned-child timeout; production runtime and provider configuration code remain unchanged.

**Tech Stack:** Rust 2024 integration tests, `tempfile`, local Git CLI, existing `yi-agent run`, existing ignored-test gate and `just` recipe.

## Global Constraints

- Work only in the existing `feat/subagent-core` linked worktree; never modify `main` directly.
- Every real workflow test is `#[ignore]` and runs only through `cargo test ... -- --ignored` or `just test-real-subagent`; normal tests and CI must make no real provider request.
- Each test uses a fresh `TempDir` repository, `git init`, a local baseline commit, and its own unique file names/markers; no fixture, worktree, commit, or marker is shared across tests.
- The test binary is built from `feat/subagent-core/yi-agent-rs`; `yi-agent --workdir` points only to the temporary repository.
- Resolve credentials exclusively through `resolve_real_llm_test_config`; return successfully with `SKIPPED: no real LLM API key configured` only when that resolver returns `Ok(None)`. Propagate explicit incomplete-configuration errors without printing key values.
- Apply real configuration only to the spawned `yi-agent` command through `RealLlmTestConfig::apply_to_command`; do not write or modify `~/.yi-agent/.env`.
- Every spawned agent command has `Duration::from_secs(300)` timeout via `run_command_with_timeout`; failure output must not interpolate the command environment or secret configuration.
- Assert filesystem/Git structure, not exact assistant prose. Provider cost is accepted; execute real tests serially with `--test-threads=1`.
- Before each commit run `cd yi-agent-rs && cargo fmt --all`; update project-management evidence and its literal README completion count in the same feature commit.

---

## File structure

| File | Responsibility |
|---|---|
| Modify `yi-agent-rs/crates/yi-agent/tests/subagent_real_e2e.rs` | Disposable Git setup helpers plus the three new ignored real-provider subagent workflow tests. |
| Modify `yi-agent-rs/justfile` | Clarify that the existing serial recipe runs all four ignored subagent regressions. |
| Modify `docs/project-management/subagent-runtime.md` | Replace the old smoke-only criterion with all four real workflow assertions and their command. |
| Modify `docs/project-management/README.md` | Recalculate the literal `subagent-runtime` completed/total count if adding/replacing a feature row changes it. |

## Task 1: Establish disposable Git and agent-command helpers

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/tests/subagent_real_e2e.rs`

**Interfaces:**
- Consumes: `RealLlmTestConfig`, `resolve_real_llm_test_config`, `run_command_with_timeout`, `yi_agent_bin`, `tempfile::TempDir`, and `std::process::Command`.
- Produces: test-local `real_config_or_skip() -> Option<RealLlmTestConfig>`, `git(repository: &Path, arguments: &[&str]) -> String`, `temporary_repository() -> TempDir`, and `run_real_subagent(config: &RealLlmTestConfig, repository: &Path, prompt: &str) -> Output`.

- [ ] **Step 1: Write helper-focused deterministic tests first**

Add non-ignored unit tests in the existing integration-test file for local helper behavior that does not contact a provider. The repository helper test must prove the fixture has a clean baseline commit; the Git helper test must prove it reads the baseline file:

```rust
#[test]
fn temporary_repository_has_a_clean_baseline_commit() {
    let repository = temporary_repository();
    assert!(!git(repository.path(), &["rev-parse", "HEAD"]).trim().is_empty());
    assert_eq!(git(repository.path(), &["status", "--porcelain"]), "");
    assert_eq!(git(repository.path(), &["show", "HEAD:README.md"]), "real subagent fixture\n");
}
```

- [ ] **Step 2: Run the new helper test and verify RED**

Run:

```bash
cd yi-agent-rs && cargo test -p yi-agent --test subagent_real_e2e temporary_repository_has_a_clean_baseline_commit
```

Expected: compilation fails because the named helper does not exist.

- [ ] **Step 3: Implement only the disposable helpers**

Implement `temporary_repository()` by creating `TempDir`, writing `README.md`, then running `git init`, `git add README.md`, and `git -c user.name=Yi -c user.email=yi@example.test commit -m baseline`. Implement `git()` with `Command::new("git").current_dir(repository).args(arguments).output()` and panic only with non-secret stdout/stderr. Implement `run_real_subagent()` with this exact argument order and dedicated command configuration:

```rust
let mut command = Command::new(yi_agent_bin());
command
    .arg("--workdir")
    .arg(repository)
    .arg("run")
    .arg(prompt);
config.apply_to_command(&mut command);
run_command_with_timeout(&mut command, Duration::from_secs(300))
```

Keep the existing README smoke test but replace its inline directory/command construction with these helpers.

- [ ] **Step 4: Verify GREEN and commit the helper refactor**

Run:

```bash
cd yi-agent-rs && cargo fmt --all
cd yi-agent-rs && cargo test -p yi-agent --test subagent_real_e2e temporary_repository_has_a_clean_baseline_commit
```

Expected: helper test passes without a network request.

```bash
git add yi-agent-rs/crates/yi-agent/tests/subagent_real_e2e.rs
git commit -m "test: prepare isolated real subagent fixtures"
```

## Task 2: Add accepted delivery and ancestry real regression

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/tests/subagent_real_e2e.rs`

**Interfaces:**
- Consumes: `temporary_repository`, `real_config_or_skip`, `run_real_subagent`, and `git` from Task 1.
- Produces: `#[ignore] fn real_subagent_accepts_delivery_into_parent_history()`.

- [ ] **Step 1: Write the ignored structural test**

Add a test using the exact unique marker and target file below. Its prompt must direct the parent to delegate exactly one child, require the child to create and commit the file in its assigned worktree, wait for delivery, inspect it, and accept it into the parent worktree. Require final verification with `read` and Git status, but do not mandate wording of the final report.

```rust
const DELIVERY_FILE: &str = "real-subagent-delivery.txt";
const DELIVERY_MARKER: &str = "REAL_SUBAGENT_DELIVERY_MARKER_V1";

#[test]
#[ignore]
fn real_subagent_accepts_delivery_into_parent_history() {
    let Some(config) = real_config_or_skip() else { return; };
    let repository = temporary_repository();
    let output = run_real_subagent(&config, repository.path(), DELIVERY_PROMPT);
    assert!(output.status.success(), "real delivery process failed");
    assert_eq!(std::fs::read_to_string(repository.path().join(DELIVERY_FILE)).unwrap(), format!("{DELIVERY_MARKER}\n"));
    let parent_head = git(repository.path(), &["rev-parse", "HEAD"]);
    let log = git(repository.path(), &["log", "--all", "--format=%H"]);
    assert!(log.lines().any(|commit| commit == parent_head.trim()));
    assert!(git(repository.path(), &["status", "--porcelain"]).is_empty());
}
```

Before finalizing the assertion, inspect the actual durable delivery evidence available through the agent output or Git log. Assert a child-delivery commit is an ancestor of final parent `HEAD` with `git merge-base --is-ancestor <delivery_commit> HEAD`; do not merely prove that the parent made its own unrelated commit.

- [ ] **Step 2: Run the test once against the real configured provider**

Run serially:

```bash
cd yi-agent-rs && cargo test -p yi-agent --test subagent_real_e2e real_subagent_accepts_delivery_into_parent_history -- --ignored --test-threads=1
```

Expected before implementation: failure because the test is absent. After adding it, record only non-secret failure evidence needed to tune the prompt or structural evidence extraction. Do not weaken the ancestry requirement into a prose assertion.

- [ ] **Step 3: Make the minimum test-only adjustment needed for reliable evidence**

If the agent cannot discover the review/accept control from the initial wording, strengthen only `DELIVERY_PROMPT` with the explicit required lifecycle: spawn child, `wait_agent`, review delivery, accept delivery, then verify `real-subagent-delivery.txt`. If Git evidence requires a known source, parse only emitted non-secret task/delivery JSON or locate the commit via `git log --all` by the unique file change. Do not change production subagent runtime for a model-following failure.

- [ ] **Step 4: Re-run and commit**

Run:

```bash
cd yi-agent-rs && cargo fmt --all
cd yi-agent-rs && cargo test -p yi-agent --test subagent_real_e2e real_subagent_accepts_delivery_into_parent_history -- --ignored --test-threads=1
```

Expected: one passed ignored real test, file marker visible at parent `HEAD`, and a proved delivery-commit ancestry relation.

```bash
git add yi-agent-rs/crates/yi-agent/tests/subagent_real_e2e.rs
git commit -m "test: verify real subagent delivery acceptance"
```

## Task 3: Add rework correction real regression

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/tests/subagent_real_e2e.rs`

**Interfaces:**
- Consumes: Task 1 helpers and the accepted-delivery lifecycle established in Task 2.
- Produces: `#[ignore] fn real_subagent_rework_replaces_incorrect_marker()`.

- [ ] **Step 1: Write the ignored rework test**

Use a fresh temporary repository and require the parent to delegate a child that first creates `real-subagent-rework.txt` containing exactly `REAL_SUBAGENT_WRONG_MARKER_V1\n`; the parent must request rework on that delivery, wait for its successor, accept only the successor, and verify the final file contains exactly `REAL_SUBAGENT_CORRECT_MARKER_V1\n`.

```rust
let final_content = std::fs::read_to_string(repository.path().join("real-subagent-rework.txt"))
    .expect("accepted rework file");
assert_eq!(final_content, "REAL_SUBAGENT_CORRECT_MARKER_V1\n");
assert!(!final_content.contains("REAL_SUBAGENT_WRONG_MARKER_V1"));
assert!(git(repository.path(), &["status", "--porcelain"]).is_empty());
```

Capture the final parent `HEAD` and prove the accepted successor delivery commit is an ancestor as in Task 2. Also assert `git log --all -- real-subagent-rework.txt` has at least two commits, which proves the initial delivery and a changed successor history exist.

- [ ] **Step 2: Run the new real test and verify RED**

Run:

```bash
cd yi-agent-rs && cargo test -p yi-agent --test subagent_real_e2e real_subagent_rework_replaces_incorrect_marker -- --ignored --test-threads=1
```

Expected before adding the test: no matching test. After adding it, any real failure must retain the exact incorrect/correct marker assertions and report only non-secret stdout/stderr.

- [ ] **Step 3: Tune the explicit lifecycle prompt, not runtime behavior**

The prompt must name the `rework` action and state that acceptance of the initial wrong delivery is forbidden. If the model emits a correct file without a rework event/history, treat that as a test failure; adjust prompt sequencing to require child delivery, parent rework, successor delivery, then parent acceptance. Do not accept a direct parent rewrite as passing.

- [ ] **Step 4: Verify and commit**

Run:

```bash
cd yi-agent-rs && cargo fmt --all
cd yi-agent-rs && cargo test -p yi-agent --test subagent_real_e2e real_subagent_rework_replaces_incorrect_marker -- --ignored --test-threads=1
```

Expected: final parent content has only the correct marker, history has initial-plus-successor evidence, successor ancestry is proved, and no temporary fixture remains outside `TempDir`.

```bash
git add yi-agent-rs/crates/yi-agent/tests/subagent_real_e2e.rs
git commit -m "test: verify real subagent rework correction"
```

## Task 4: Add concurrent delegation and worktree-isolation real regression

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/tests/subagent_real_e2e.rs`

**Interfaces:**
- Consumes: Task 1 helpers and Task 2 delivery evidence assertion approach.
- Produces: `#[ignore] fn real_subagents_concurrently_deliver_isolated_worktrees()`.

- [ ] **Step 1: Write the ignored concurrency test**

Use a fresh repository and require the parent to issue two `spawn_agent` calls before the first `wait_agent` call. One child must create/commit `real-child-alpha.txt` with `REAL_CHILD_ALPHA_MARKER_V1\n`; the other must create/commit `real-child-beta.txt` with `REAL_CHILD_BETA_MARKER_V1\n`. The parent must wait for both, review/accept both deliveries, then read both final files.

```rust
assert_eq!(
    std::fs::read_to_string(repository.path().join("real-child-alpha.txt")).unwrap(),
    "REAL_CHILD_ALPHA_MARKER_V1\n"
);
assert_eq!(
    std::fs::read_to_string(repository.path().join("real-child-beta.txt")).unwrap(),
    "REAL_CHILD_BETA_MARKER_V1\n"
);
assert!(git(repository.path(), &["status", "--porcelain"]).is_empty());
let distinct_branches = git(repository.path(), &["branch", "--all"]);
assert!(distinct_branches.matches("yi-agent-").count() >= 2, "missing child worktree branches");
```

Prove each target file has a distinct non-baseline introducing commit and that both commits are ancestors of parent `HEAD`. This is the isolation proof: each delivery must originate from a separate branch/worktree history, not merely two parent writes.

- [ ] **Step 2: Run the test once and verify RED**

Run:

```bash
cd yi-agent-rs && cargo test -p yi-agent --test subagent_real_e2e real_subagents_concurrently_deliver_isolated_worktrees -- --ignored --test-threads=1
```

Expected before adding it: no matching test. Once present, a failure caused by sequential spawn or merged parent writes is a valid behavioral failure; preserve the independent delivery evidence requirement.

- [ ] **Step 3: Tighten only the model task contract if needed**

Use explicit prompt text: “Call `spawn_agent` twice before calling `wait_agent`; do not create either target file yourself.” Require the two exact names/markers, separate child commits, two accepts, and final verification. Do not add sleeps, polling loops, or test-side concurrent Cargo invocation.

- [ ] **Step 4: Verify and commit**

Run:

```bash
cd yi-agent-rs && cargo fmt --all
cd yi-agent-rs && cargo test -p yi-agent --test subagent_real_e2e real_subagents_concurrently_deliver_isolated_worktrees -- --ignored --test-threads=1
```

Expected: both exact files exist at final parent `HEAD`, their introducing commits are distinct and ancestor-related, and Git worktree branch evidence identifies at least two child branches.

```bash
git add yi-agent-rs/crates/yi-agent/tests/subagent_real_e2e.rs
git commit -m "test: verify real subagent worktree isolation"
```

## Task 5: Make the all-scenarios recipe and tracking criteria accurate

**Files:**
- Modify: `yi-agent-rs/justfile`
- Modify: `docs/project-management/subagent-runtime.md`
- Modify: `docs/project-management/README.md`

**Interfaces:**
- Consumes: four ignored tests in `subagent_real_e2e.rs` and existing `test-real-subagent` recipe.
- Produces: one documented serial command that runs README smoke, accepted delivery, rework correction, and concurrency/isolation tests; project-management criterion and literal completion count match the code.

- [ ] **Step 1: Write a deterministic recipe/documentation assertion or inspectable criterion**

Update the recipe comment to name all four scenarios and retain:

```make
cargo test -p yi-agent --test subagent_real_e2e -- --ignored --test-threads=1
```

Update `subagent-runtime.md` to name the exact file, each of the four test names, `just test-real-subagent`, isolated `TempDir` Git repositories, and the no-key skip/explicit incomplete-config behavior. Count literal `- [x]`/`- [-]` entries before changing the `README.md` index number.

- [ ] **Step 2: Verify normal tests remain network-free**

Run without `--ignored`:

```bash
cd yi-agent-rs && cargo test -p yi-agent --test subagent_real_e2e
```

Expected: all real workflow tests are reported ignored and no provider call is attempted.

- [ ] **Step 3: Verify configuration gates and full real recipe**

First verify skip/failure behavior in isolated environments, ensuring no shell command prints values:

```bash
cd yi-agent-rs && env -i PATH="$PATH" HOME="$(mktemp -d)" cargo test -p yi-agent --test subagent_real_e2e -- --ignored --test-threads=1
cd yi-agent-rs && env -i PATH="$PATH" HOME="$(mktemp -d)" YI_AGENT_REAL_LLM_PROVIDER=anthropic cargo test -p yi-agent --test subagent_real_e2e -- --ignored --test-threads=1; test $? -ne 0
```

Then, only with the intentionally configured user-level dedicated provider, run:

```bash
cd yi-agent-rs && just test-real-subagent
```

Expected: four passed tests, zero failures, with no API key in captured output. If any scenario fails, retain the failing structural assertion, diagnose the real output, and return to the task that owns that scenario.

- [ ] **Step 4: Run final scoped quality gate and commit tracking**

Run serially:

```bash
cd yi-agent-rs && cargo fmt --all
cd yi-agent-rs && cargo test -p yi-agent --test subagent_real_e2e
cd yi-agent-rs && cargo test -p yi-agent
cd yi-agent-rs && cargo clippy -p yi-agent --all-targets -- -D warnings
cd .. && git diff --check main...HEAD
git status --short --branch
```

Expected: deterministic tests and lint pass; normal `subagent_real_e2e` run shows the four real tests ignored; `git diff --check` passes. Commit only the changed recipe and tracking files:

```bash
git add yi-agent-rs/justfile docs/project-management/subagent-runtime.md docs/project-management/README.md
git commit -m "docs: record real subagent workflow coverage"
```
