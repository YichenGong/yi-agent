# Web Global Default Scope Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Open the web configuration editor on the global scope by default when it is available, with global shown before local.

**Architecture:** Keep the API unchanged. The embedded HTML owns the scope selector state, so reorder its controls and initialize `currentScope` to `global`; `loadConfig()` will explicitly fall back to `local` when the server provides no global path. An integration test fetches the HTML endpoint and verifies this source-level UI contract.

**Tech Stack:** Rust, Axum, Tokio integration tests, embedded HTML and JavaScript.

---

### Task 1: Lock Down the Default Scope Contract

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-web/tests/api_test.rs`
- Test: `yi-agent-rs/crates/yi-agent-web/tests/api_test.rs`

- [x] **Step 1: Write the failing test**

Add `index_html_defaults_to_global_scope_before_local_scope` after `index_html_returns_html`. It fetches `/`, then asserts that the index of `id="scopeGlobal"` is less than `id="scopeLocal"`, that the HTML contains `let currentScope = 'global';`, and that the unavailable-global branch assigns the local scope and active state.

- [x] **Step 2: Run the test to verify it fails**

Run `cargo test -p yi-agent-web --test api_test index_html_defaults_to_global_scope_before_local_scope` from `yi-agent-rs`. Expected: FAIL because the local button appears before the global button and the initial scope is `local`.

### Task 2: Select Global First and Fall Back Safely

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-web/src/assets/index.html:256`
- Modify: `yi-agent-rs/crates/yi-agent-web/src/assets/index.html:279`
- Modify: `yi-agent-rs/crates/yi-agent-web/src/assets/index.html:306`
- Test: `yi-agent-rs/crates/yi-agent-web/tests/api_test.rs`

- [x] **Step 1: Make the minimal implementation**

Render `scopeGlobal` before `scopeLocal`, assign `active` to `scopeGlobal`, and initialize `currentScope` to `global`. In the no-global branch of `loadConfig()`, set `currentScope = 'local'`, add `active` to `scopeLocal`, and remove `active` from `scopeGlobal` before hiding the selector.

- [x] **Step 2: Run the focused test to verify it passes**

Run `cargo test -p yi-agent-web --test api_test index_html_defaults_to_global_scope_before_local_scope` from `yi-agent-rs`. Expected: PASS.

- [x] **Step 3: Run the crate integration suite**

Run `cargo test -p yi-agent-web --test api_test` from `yi-agent-rs`. Expected: PASS with all `api_test` tests passing.

- [x] **Step 4: Format and commit the implementation**

Run `cargo fmt --all` from `yi-agent-rs`, then commit the HTML and test changes as `fix(web): default configuration scope to global`.

### Task 3: Update Project Progress

**Files:**
- Modify: `docs/project-management/yi-agent-web.md`
- Modify: `README.md`

- [x] **Step 1: Identify the matching web configuration requirement**

Read `docs/project-management/yi-agent-web.md` and find the existing WebUI configuration requirement that covers global and local scope editing.

- [x] **Step 2: Record the completed behavior with a verifier**

Add a completed requirement entry for the global-first default, citing `yi-agent-rs/crates/yi-agent-web/tests/api_test.rs::index_html_defaults_to_global_scope_before_local_scope` as the executable verifier. Update the matching completed/total count in `README.md` only if this is a new tracked requirement.

- [x] **Step 3: Commit the progress documentation**

Commit the project-management documentation as `docs: track web global default scope`. Expected: it records a verifiable completed requirement and contains no unrelated files.
