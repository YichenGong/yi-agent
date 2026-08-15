# Web Mock Proxy Isolation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make WebFetch and Bocha wiremock tests independent of environment proxy settings while preserving production proxy behavior.

**Architecture:** Production constructors keep their current default reqwest client. Test-only constructors build otherwise-identical clients with `.no_proxy()`, and each local mock test uses them. No process environment variables are changed.

**Tech Stack:** Rust 2024, reqwest 0.12, wiremock, tokio.

---

### Task 1: Define test-only no-proxy constructors

**Files:**
- Modify and test: `yi-agent-rs/crates/yi-agent-tools/src/web/fetch.rs`
- Modify and test: `yi-agent-rs/crates/yi-agent-tools/src/web/bocha.rs`

- [x] Add `#[cfg(test)] fn new_for_test()` to `WebFetchTool`; it builds the same timeout, redirect, and user-agent configuration as `new()`, plus `.no_proxy()`.
- [x] Add `#[cfg(test)] fn with_base_url_for_test(api_key: String, base_url: String)` to `BochaSearchProvider`; it builds the same timeout configuration as `with_base_url`, plus `.no_proxy()`.
- [x] Replace each wiremock test's production constructor with the matching test-only constructor.

### Task 2: Prove the fix under the proxy environment

**Files:**
- Test: `yi-agent-rs/crates/yi-agent-tools/src/web/fetch.rs`
- Test: `yi-agent-rs/crates/yi-agent-tools/src/web/bocha.rs`

- [x] Run `cargo test -p yi-agent-tools web::fetch::tests::fetch_plain_text` while proxy variables remain set; it passes.
- [x] Run `cargo test -p yi-agent-tools web::bocha::tests::search_returns_results` while proxy variables remain set; it passes.
- [x] Run `cargo test -p yi-agent-tools --lib`; all 156 library tests pass.

### Task 3: Record and deliver the fix

**Files:**
- Modify: `docs/project-management/yi-agent-tools.md`

- [x] Add a verified completion criterion explaining that web mock clients bypass environment proxies only in tests, verified by `cargo test -p yi-agent-tools --lib`.
- [x] Run `cd yi-agent-rs && cargo fmt --all && cargo test -p yi-agent-tools --lib`.
- [ ] Commit `fix(tools): isolate web mocks from proxy settings`.
