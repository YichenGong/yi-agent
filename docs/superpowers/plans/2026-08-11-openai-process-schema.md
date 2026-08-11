# OpenAI Process Selector Schema Compatibility Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make managed-process tool schemas acceptable to OpenAI-compatible function-calling endpoints without weakening the existing exactly-one selector validation.

**Architecture:** `process_read` and `process_kill` share `process_selector_schema` in `yi-agent-tools`. Remove only the top-level `oneOf`; `selector_from_args` remains the shared runtime guard for zero or multiple selectors. A unit test checks each public tool schema rather than introducing provider-specific schema rewriting.

**Tech Stack:** Rust, `serde_json`, Tokio unit tests, Cargo.

---

### Task 1: Establish the schema regression

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/process/tools.rs`
- Test: `yi-agent-rs/crates/yi-agent-tools/src/process/tools.rs`

- [ ] **Step 1: Write the failing test**

Add this test to the existing `tests` module:

```rust
#[test]
fn process_selector_schemas_are_openai_compatible() {
    let temp = TempDir::new().unwrap();
    let manager = ProcessManager::new(temp.path().to_path_buf());
    let tools: Vec<Box<dyn Tool>> = vec![
        Box::new(ProcessReadTool::new(manager.clone())),
        Box::new(ProcessKillTool::new(manager)),
    ];

    for tool in tools {
        let schema = tool.schema();
        assert_eq!(schema["type"], "object", "{}", tool.name());
        for keyword in ["oneOf", "anyOf", "allOf", "enum", "const", "not"] {
            assert!(schema.get(keyword).is_none(), "{} contains {keyword}", tool.name());
        }
    }
}
```

- [ ] **Step 2: Run the regression test and verify it fails**

Run: `cargo test -p yi-agent-tools --lib process::tools::tests::process_selector_schemas_are_openai_compatible -- --exact`

Expected: FAIL because both schemas contain a top-level `oneOf`.

### Task 2: Remove the unsupported top-level composition

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/process/tools.rs`

- [ ] **Step 1: Apply the smallest implementation change**

Replace the return value in `process_selector_schema` with:

```rust
serde_json::json!({
    "type": "object",
    "properties": properties
})
```

Do not change `selector_from_args`; it continues to reject missing and ambiguous selectors at tool execution time.

- [ ] **Step 2: Run the regression test and verify it passes**

Run: `cargo test -p yi-agent-tools --lib process::tools::tests::process_selector_schemas_are_openai_compatible -- --exact`

Expected: PASS.

- [ ] **Step 3: Run the existing managed-process tests**

Run: `cargo test -p yi-agent-tools --lib process::tools::tests::`

Expected: PASS, including `process_tools_start_list_read_kill` and selector validation coverage.

### Task 3: Record completion and deliver the fix

**Files:**
- Modify: `docs/project-management/yi-agent-tools.md`

- [ ] **Step 1: Update the managed-process feature criterion**

Extend the existing Managed process tools bullet to state that `process_read` and `process_kill` emit OpenAI-compatible top-level object schemas without composition keywords, verified by the new exact test command.

- [ ] **Step 2: Format and run final verification**

Run:

```bash
cd yi-agent-rs
cargo fmt --all
cargo test -p yi-agent-tools --lib process::tools::tests::
just fmt-check
```

Expected: all commands exit 0.

- [ ] **Step 3: Commit the implementation**

```bash
git add yi-agent-rs/crates/yi-agent-tools/src/process/tools.rs docs/project-management/yi-agent-tools.md
git commit -m "fix: make process tool schemas provider-compatible"
```
