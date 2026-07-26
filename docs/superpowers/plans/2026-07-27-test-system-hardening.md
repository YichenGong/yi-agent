# Test System Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make configuration and real-LLM tests isolated, deterministic about skip status, and safe to run locally.

**Architecture:** A scoped environment-variable guard will save and restore test process state while a poison-tolerant mutex serializes all environment-mutating unit tests. Real-LLM recipes will select one configured provider, run one test at a time, and explicitly print `SKIPPED` when no supported configuration exists. Agent subprocesses will enforce a deadline by retaining the actual `Child`, never by killing a stale PID.

**Tech Stack:** Rust std process/environment APIs, Cargo integration tests, Just.

---

### Task 1: Isolate configuration tests

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/config.rs`

- [ ] **Step 1: Write failing regression tests**

Add tests that set `MODEL_API_KEY` before invoking the dotenv fixture and assert that the fixture value wins only within the scoped test, and that the original value is restored after the scope ends.

- [ ] **Step 2: Run the focused tests to verify failure**

Run: `MODEL_API_KEY=host-value cargo test -p yi-agent config::tests::load_reads_dotenv_file -- --exact`

Expected: FAIL because dotenvy preserves the host environment variable.

- [ ] **Step 3: Implement scoped environment restoration**

Add a test-only `EnvVarGuard` that snapshots named variables, supports set/remove operations, restores values in `Drop`, and obtains `ENV_TEST_MUTEX` with poison recovery. Use it in every configuration test that changes process environment.

- [ ] **Step 4: Run the focused tests to verify success**

Run: `MODEL_API_KEY=host-value cargo test -p yi-agent config::tests::load_reads_dotenv_file -- --exact`

Expected: PASS and no environment mutation leaks to later tests.

### Task 2: Make real-test execution truthful and serial

**Files:**
- Modify: `yi-agent-rs/justfile`
- Modify: `yi-agent-rs/crates/yi-agent/tests/e2e_real.rs`
- Modify: `yi-agent-rs/crates/yi-agent/tests/e2e_complex.rs`
- Modify: `yi-agent-rs/crates/yi-agent-llm/tests/real_integration.rs`

- [ ] **Step 1: Write failing recipe-level checks**

Run each `just test-real-*` recipe with a temporary empty `.env` and assert its output contains `SKIPPED: no supported real-LLM configuration` and exits zero. Run the corresponding ignored test binaries with `--test-threads=1` in dry compilation mode.

- [ ] **Step 2: Implement a single configuration contract**

Require `MODEL_API_KEY` plus `YI_AGENT_PROVIDER` for agent E2E tests; allow provider smoke tests only when their provider-specific key and optional model environment are supplied. Recipes print the selected target or a single explicit skip, then pass `--test-threads=1`.

- [ ] **Step 3: Remove process-global mutations from real provider tests**

Pass explicit `api_key: None` to authentication construction tests without deleting environment variables, and make provider-dependent tests explicitly report missing credentials rather than silently looking successful in an aggregate run.

- [ ] **Step 4: Verify recipe and compilation behavior**

Run: `just test-real-llm`, `just test-real-e2e`, `just test-real-complex`, and Cargo `--no-run` for all modified integration targets.

Expected: each unconfigured recipe emits one `SKIPPED` line and exits zero; no real network call occurs.

### Task 3: Replace unsafe subprocess timeout and strengthen deterministic assertions

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/tests/common/mod.rs`
- Modify: `yi-agent-rs/crates/yi-agent/tests/e2e_real.rs`
- Modify: `yi-agent-rs/crates/yi-agent/tests/e2e_complex.rs`

- [ ] **Step 1: Write failing helper tests**

Add a helper test using a short-lived command and a deliberately sleeping command. Assert the former returns normally and the latter is killed by the owned child handle with a timeout error; no detached thread is created.

- [ ] **Step 2: Implement owned-child deadline polling**

Use `Child::try_wait` in a bounded polling loop, call `child.kill()` only when its own deadline expires, then collect output. Apply the helper to every E2E child process.

- [ ] **Step 3: Strengthen task outcome checks**

Validate the exact JSON data array, run Python syntax validation and deterministic function checks, require `echo hello` to appear in its matching bash tool result, and require auto-compaction to occur when the test claims to test it.

- [ ] **Step 4: Run focused and workspace verification**

Run: `cargo test -p yi-agent --test e2e_real --test e2e_complex`, then `cargo test --workspace --all-features`.

Expected: all non-ignored tests pass with no real API calls.
