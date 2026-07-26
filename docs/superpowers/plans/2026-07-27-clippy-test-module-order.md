# Clippy Test Module Order Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove the strict Clippy `items_after_test_module` violation without changing E2E test helper behavior.

**Architecture:** Reorder one existing test module so all reusable helper functions precede it. No APIs, test assertions, timeouts, or process handling change.

**Tech Stack:** Rust, Cargo, Clippy.

---

### Task 1: Place the Common-Helper Test Module Last

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/tests/common/mod.rs:71-93`
- Test: `yi-agent-rs/crates/yi-agent/tests/common/mod.rs:80`

- [ ] **Step 1: Verify the current strict lint failure**

  Run: `cd yi-agent-rs && cargo clippy -p yi-agent --test e2e_complex -- -D warnings`

  Expected: failure from `clippy::items_after_test_module` because `mod tests` precedes `wait_for_child`.

- [ ] **Step 2: Move the unchanged test module after all helper functions**

  Remove this block from its current location:

  ```rust
  #[cfg(test)]
  mod tests {
      use super::*;

      #[test]
      fn owned_child_timeout_reports_timeout() {
          let child = Command::new("sh")
              .args(["-c", "sleep 1"])
              .spawn()
              .expect("spawn sleeping child");

          let err = wait_for_child(child, Duration::from_millis(20)).expect_err("should time out");
          assert!(err.contains("timed out"), "unexpected error: {err}");
      }
  }
  ```

  Append the identical block after `run_agent_with_timeout` at the end of the same file.

- [ ] **Step 3: Run format, compile, test, and strict lint verification**

  Run: `cd yi-agent-rs && cargo fmt --all && cargo test -p yi-agent --test e2e_complex --no-run && cargo test -p yi-agent --test e2e_complex common::tests::owned_child_timeout_reports_timeout && cargo clippy -p yi-agent --all-targets --all-features -- -D warnings`

  Expected: all commands succeed; the timeout test passes and strict Clippy reports no warnings.

- [ ] **Step 4: Commit the isolated fix**

  ```bash
  git add yi-agent-rs/crates/yi-agent/tests/common/mod.rs
  git commit -m "fix: satisfy clippy test module order"
  ```
