# 分级测试系统:复杂 one-shot 任务测试 实现计划

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 新增 Tier 3 复杂 one-shot 任务测试,验证 agent 能完成多步骤生成任务(个人网站、Python 脚本、数据转换、bug 修复)。

**Architecture:** 新建 `tests/e2e_complex.rs` 与共享 helper 模块 `tests/common/mod.rs`,复用 `yi-agent run --json` CLI 通过子进程执行 agent,用 `TempDir` 隔离,300s 超时,结构性断言检查产出文件。

**Tech Stack:** Rust, tokio, tempfile, serde_json, std::process::Command

---

## Task 1: 提取共享 helper 到 `tests/common/mod.rs`

**Files:**
- Create: `yi-agent-rs/crates/yi-agent/tests/common/mod.rs`
- Modify: `yi-agent-rs/crates/yi-agent/tests/e2e_real.rs`

**Step 1: 创建 `tests/common/mod.rs`**

从 `e2e_real.rs` 提取以下 helper 函数到新文件:

```rust
//! 共享 helper:供 e2e_real.rs 和 e2e_complex.rs 复用。

use std::path::PathBuf;
use std::process::Command;

/// Path to the compiled yi-agent binary.
pub fn yi_agent_bin() -> PathBuf {
    option_env!("CARGO_BIN_EXE_yi-agent")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/debug/yi-agent"))
}

/// 检查是否有任何可用的 API key 配置。
pub fn has_api_key() -> bool {
    let has_provider_key = std::env::var("ANTHROPIC_API_KEY")
        .or_else(|_| std::env::var("OPENAI_API_KEY"))
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    let has_config_key = std::env::var("MODEL_API_KEY")
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    has_provider_key || has_config_key
}

/// 无 key 时打印 skip 并返回 false。
pub fn skip_if_no_key() -> bool {
    if !has_api_key() {
        eprintln!("skip: no API key");
        false
    } else {
        true
    }
}

/// 返回可用于传给 yi-agent 的 --api-key 值。
pub fn resolve_api_key() -> Option<String> {
    std::env::var("MODEL_API_KEY")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::env::var("ANTHROPIC_API_KEY")
                .ok()
                .filter(|s| !s.is_empty())
        })
        .or_else(|| {
            std::env::var("OPENAI_API_KEY")
                .ok()
                .filter(|s| !s.is_empty())
        })
}

/// 从一行 JSONL 提取 AgentEvent 的 variant 名。
pub fn event_variant(line: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if let Some(s) = v.as_str() {
        return Some(s.to_string());
    }
    if let Some(obj) = v.as_object() {
        return obj.keys().next().cloned();
    }
    None
}

/// 解析 JSONL 为 Vec<serde_json::Value>。
pub fn parse_events(jsonl: &str) -> Vec<serde_json::Value> {
    jsonl
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("valid JSONL line"))
        .collect()
}

/// 检查事件流中是否有 Done 事件。
pub fn has_done_event(events: &[serde_json::Value]) -> bool {
    events.iter().any(|v| {
        v.as_str() == Some("Done")
            || v.as_object().map(|o| o.contains_key("Done")).unwrap_or(false)
    })
}
```

**Step 2: 重构 `e2e_real.rs` 使用共享 helper**

将 `e2e_real.rs` 顶部的 helper 函数(`yi_agent_bin`、`has_api_key`、`resolve_api_key`、`event_variant`)删除,替换为:

```rust
mod common;
use common::{event_variant, has_api_key, resolve_api_key, yi_agent_bin};
```

测试函数体不变,仅将 `yi_agent_bin()` 调用保持不变(函数名一致),`has_api_key()`、`resolve_api_key()`、`event_variant()` 调用也不变。

**Step 3: 验证编译**

Run: `cd yi-agent-rs && cargo test -p yi-agent --test e2e_real --no-run`
Expected: 编译成功(测试仍为 `#[ignore]`,不会运行)

**Step 4: 验证非 ignored 测试仍跑通**

Run: `cd yi-agent-rs && cargo test -p yi-agent --test e2e_real`
Expected: `0 passed; 0 failed; 5 ignored`(所有测试都被 ignore,不运行)

**Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent/tests/common/mod.rs crates/yi-agent/tests/e2e_real.rs
git commit -m "refactor: extract e2e test helpers to tests/common/mod.rs"
```

---

## Task 2: 新建 `tests/e2e_complex.rs` 骨架 + 场景 1(个人网站)

**Files:**
- Create: `yi-agent-rs/crates/yi-agent/tests/e2e_complex.rs`

**Step 1: 创建 `e2e_complex.rs` 含场景 1 测试**

```rust
//! 复杂 one-shot 任务测试:验证 agent 能完成多步骤生成任务。
//! 全部 #[ignore]'d; run with: cargo test -p yi-agent --test e2e_complex -- --ignored
//!
//! 配置源:父进程环境变量(由 justfile recipe 从 .env 加载)。

mod common;
use common::{has_done_event, parse_events, skip_if_no_key, yi_agent_bin};

use std::process::Command;
use std::time::Duration;

const PROMPT_WEBSITE: &str = "Create a single-page personal website. Write the complete HTML (with inline CSS) to output/index.html. The page should include a header, an 'About' section, and a footer. Use the write tool to create the file.";

#[test]
#[ignore]
fn complex_personal_website() {
    if !skip_if_no_key() {
        return;
    }

    let tmp = tempfile::TempDir::new().expect("tempdir");
    std::fs::create_dir_all(tmp.path().join("output")).expect("create output dir");

    let output = Command::new(yi_agent_bin())
        .arg("--workdir")
        .arg(tmp.path())
        .arg("run")
        .arg("--json")
        .arg(PROMPT_WEBSITE)
        .output()
        .expect("failed to spawn yi-agent");

    assert!(
        output.status.success(),
        "yi-agent run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let events = parse_events(&stdout);
    assert!(has_done_event(&events), "no Done event, stdout: {stdout}");

    // 结构性断言
    let html_path = tmp.path().join("output/index.html");
    assert!(html_path.exists(), "index.html not created");
    let html = std::fs::read_to_string(&html_path).expect("read index.html");
    assert!(
        html.len() > 500,
        "index.html too small: {} bytes",
        html.len()
    );
    let lower = html.to_lowercase();
    assert!(lower.contains("<html"), "missing <html tag");
    assert!(lower.contains("<body"), "missing <body tag");
    assert!(lower.contains("about"), "missing About section");
    assert!(lower.contains("<footer"), "missing <footer tag");
}
```

**Step 2: 验证编译**

Run: `cd yi-agent-rs && cargo test -p yi-agent --test e2e_complex --no-run`
Expected: 编译成功

**Step 3: 验证无 key 时 skip**

Run: `cd yi-agent-rs && cargo test -p yi-agent --test e2e_complex -- --ignored`
Expected: `0 passed; 0 failed; 1 ignored`(无 key 时测试函数 early return,但 cargo 仍计为 ignored)

注意:这里测试实际会运行(因为加了 `--ignored`),但 `skip_if_no_key()` 会在无 key 时 early return,测试以 `passed` 状态结束。如果有 key,测试会真实调用 LLM。

**Step 4: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent/tests/e2e_complex.rs
git commit -m "feat: add complex_personal_website e2e test (Tier 3)"
```

---

## Task 3: 场景 2 — Python 工具脚本

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/tests/e2e_complex.rs`

**Step 1: 追加 `complex_python_script` 测试**

在 `e2e_complex.rs` 末尾追加:

```rust
const PROMPT_PYTHON: &str = "Write a Python function called `sort_list` that takes a list and returns it sorted in ascending order. Write it to output/sort.py. The file should be a valid Python module with a `if __name__ == '__main__'` guard that demonstrates the function.";

#[test]
#[ignore]
fn complex_python_script() {
    if !skip_if_no_key() {
        return;
    }

    let tmp = tempfile::TempDir::new().expect("tempdir");
    std::fs::create_dir_all(tmp.path().join("output")).expect("create output dir");

    let output = Command::new(yi_agent_bin())
        .arg("--workdir")
        .arg(tmp.path())
        .arg("run")
        .arg("--json")
        .arg(PROMPT_PYTHON)
        .output()
        .expect("failed to spawn yi-agent");

    assert!(
        output.status.success(),
        "yi-agent run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let events = parse_events(&stdout);
    assert!(has_done_event(&events), "no Done event, stdout: {stdout}");

    // 结构性断言(不执行产出代码)
    let py_path = tmp.path().join("output/sort.py");
    assert!(py_path.exists(), "sort.py not created");
    let py = std::fs::read_to_string(&py_path).expect("read sort.py");
    assert!(py.len() > 100, "sort.py too small: {} bytes", py.len());
    assert!(py.contains("def sort_list"), "missing def sort_list");
    assert!(py.contains("__main__"), "missing __main__ guard");
}
```

**Step 2: 验证编译**

Run: `cd yi-agent-rs && cargo test -p yi-agent --test e2e_complex --no-run`
Expected: 编译成功

**Step 3: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent/tests/e2e_complex.rs
git commit -m "feat: add complex_python_script e2e test"
```

---

## Task 4: 场景 3 — 数据转换

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/tests/e2e_complex.rs`

**Step 1: 追加 `complex_data_transformation` 测试**

```rust
const PROMPT_DATA: &str = "Read the file input/data.json, extract all `name` fields, convert them to uppercase, and write the result as a JSON array to output/results.json.";

#[test]
#[ignore]
fn complex_data_transformation() {
    if !skip_if_no_key() {
        return;
    }

    let tmp = tempfile::TempDir::new().expect("tempdir");
    std::fs::create_dir_all(tmp.path().join("input")).expect("create input dir");
    std::fs::create_dir_all(tmp.path().join("output")).expect("create output dir");
    std::fs::write(
        tmp.path().join("input/data.json"),
        r#"[{"name":"alice","age":30},{"name":"bob","age":25},{"name":"charlie","age":35}]"#,
    )
    .expect("write data.json");

    let output = Command::new(yi_agent_bin())
        .arg("--workdir")
        .arg(tmp.path())
        .arg("run")
        .arg("--json")
        .arg(PROMPT_DATA)
        .output()
        .expect("failed to spawn yi-agent");

    assert!(
        output.status.success(),
        "yi-agent run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let events = parse_events(&stdout);
    assert!(has_done_event(&events), "no Done event, stdout: {stdout}");

    // 结构性断言
    let results_path = tmp.path().join("output/results.json");
    assert!(results_path.exists(), "results.json not created");
    let content = std::fs::read_to_string(&results_path).expect("read results.json");

    // 必须是合法 JSON 数组
    let parsed: serde_json::Value =
        serde_json::from_str(&content).expect("results.json is not valid JSON");
    let arr = parsed
        .as_array()
        .expect("results.json should be a JSON array");
    assert_eq!(arr.len(), 3, "should have 3 elements, got: {content}");

    // 内容包含大写名字
    let upper = content.to_uppercase();
    assert!(upper.contains("ALICE"), "missing ALICE: {content}");
    assert!(upper.contains("BOB"), "missing BOB: {content}");
    assert!(upper.contains("CHARLIE"), "missing CHARLIE: {content}");
}
```

**Step 2: 验证编译**

Run: `cd yi-agent-rs && cargo test -p yi-agent --test e2e_complex --no-run`
Expected: 编译成功

**Step 3: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent/tests/e2e_complex.rs
git commit -m "feat: add complex_data_transformation e2e test"
```

---

## Task 5: 场景 4 — Bug 修复

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/tests/e2e_complex.rs`

**Step 1: 追加 `complex_bug_fix` 测试**

```rust
const PROMPT_BUGFIX: &str = "The file buggy.py contains a Python function with a bug. Read it, identify the bug, fix it, and write the fixed version to output/fixed.py. Do not just copy the original — fix the bug.";

#[test]
#[ignore]
fn complex_bug_fix() {
    if !skip_if_no_key() {
        return;
    }

    let tmp = tempfile::TempDir::new().expect("tempdir");
    std::fs::create_dir_all(tmp.path().join("output")).expect("create output dir");
    std::fs::write(
        tmp.path().join("buggy.py"),
        "def add(a, b):\n    return a - b   # BUG: should be +\n\nif __name__ == \"__main__\":\n    print(add(2, 3))\n",
    )
    .expect("write buggy.py");

    let output = Command::new(yi_agent_bin())
        .arg("--workdir")
        .arg(tmp.path())
        .arg("run")
        .arg("--json")
        .arg(PROMPT_BUGFIX)
        .output()
        .expect("failed to spawn yi-agent");

    assert!(
        output.status.success(),
        "yi-agent run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let events = parse_events(&stdout);
    assert!(has_done_event(&events), "no Done event, stdout: {stdout}");

    // 结构性断言
    let fixed_path = tmp.path().join("output/fixed.py");
    assert!(fixed_path.exists(), "fixed.py not created");
    let fixed = std::fs::read_to_string(&fixed_path).expect("read fixed.py");
    assert!(fixed.len() > 50, "fixed.py too small: {} bytes", fixed.len());
    assert!(fixed.contains("def add"), "missing def add");
    assert!(fixed.contains('+'), "missing + (fix should contain addition)");
    assert!(
        !fixed.contains("return a - b"),
        "original bug line 'return a - b' should be replaced"
    );
}
```

**Step 2: 验证编译**

Run: `cd yi-agent-rs && cargo test -p yi-agent --test e2e_complex --no-run`
Expected: 编译成功

**Step 3: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent/tests/e2e_complex.rs
git commit -m "feat: add complex_bug_fix e2e test"
```

---

## Task 6: 更新 justfile

**Files:**
- Modify: `yi-agent-rs/justfile`

**Step 1: 新增 `test-real-complex` recipe 并更新 `test-real-all`**

在 `test-real-e2e` recipe 后(`test-real-all` 前)插入:

```makefile
# 跑复杂 one-shot 任务测试(Tier 3)
# 配置源:yi-agent-rs/.env(优先,强制覆盖 shell 环境变量)
test-real-complex:
    #!/usr/bin/env bash
    set -e
    unset ANTHROPIC_API_KEY OPENAI_API_KEY MODEL_API_KEY MODEL_API_URL YI_AGENT_PROVIDER YI_AGENT_MODEL
    if [ -f .env ]; then
        set -a; . ./.env; set +a
    fi
    if [ -z "$ANTHROPIC_API_KEY" ] && [ -z "$OPENAI_API_KEY" ] && [ -z "$MODEL_API_KEY" ]; then
        echo "skip: no API key set (.env)"
        exit 0
    fi
    cargo test -p yi-agent --test e2e_complex -- --ignored
```

将 `test-real-all` 改为:

```makefile
# 跑所有真实 LLM 测试(Tier 1 + 2 + 3)
test-real-all: test-real-llm test-real-e2e test-real-complex
    @echo "All real LLM tests passed"
```

**Step 2: 验证 justfile 语法**

Run: `cd yi-agent-rs && just --list`
Expected: 列表含 `test-real-complex`

**Step 3: 验证无 key 时 skip**

Run: `cd yi-agent-rs && just test-real-complex`
Expected: 输出 `skip: no API key set (.env)`,exit 0

**Step 4: Commit**

```bash
git add yi-agent-rs/justfile
git commit -m "ci: add test-real-complex recipe for Tier 3 tests"
```

---

## Task 7: 更新 CLAUDE.md

**Files:**
- Modify: `CLAUDE.md`

**Step 1: 在 `## 真实 LLM 测试` 章节后追加 `## 分级测试系统` 小节**

```markdown
## 分级测试系统

- **Tier 0 (Mock)**: `cargo test` / `just test` — wiremock,总是跑,无 API key
- **Tier 1 (Provider smoke)**: `just test-real-llm` — SSE 解析、鉴权
- **Tier 2 (Simple e2e)**: `just test-real-e2e` — 单轮文本、单工具调用
- **Tier 3 (Complex one-shot)**: `just test-real-complex` — 多步骤生成任务
  (个人网站、Python 脚本、数据转换、bug 修复)
- `just test-real-all` 跑 Tier 1 + 2 + 3
- 复杂测试用 `tempfile::TempDir` 隔离,300s 超时,结构性断言(文件存在/大小/标记)
- 复杂测试同样是 `#[ignore]` gate,CI 不跑
- 测试文件: `yi-agent-rs/crates/yi-agent/tests/e2e_complex.rs`
- 共享 helper: `yi-agent-rs/crates/yi-agent/tests/common/mod.rs`
```

**Step 2: Commit**

```bash
git add CLAUDE.md
git commit -m "docs: add graded test system section to CLAUDE.md"
```

---

## Task 8: 更新 project-management 文档

**Files:**
- Modify: `docs/project-management/yi-agent-run.md`
- Modify: `docs/project-management/README.md`(如需要)

**Step 1: 在 `yi-agent-run.md` 的 Features 列表末尾追加**

```markdown
- [x] 复杂 one-shot 任务测试(Tier 3)— `crates/yi-agent/tests/e2e_complex.rs` 4 个场景 — [设计](../plans/2026-07-26-graded-test-system-design.md)
```

**Step 2: Commit**

```bash
git add docs/project-management/yi-agent-run.md
git commit -m "docs: update yi-agent-run module status with Tier 3 tests"
```

---

## Task 9: 最终验证

**Step 1: 跑 fmt-check**

Run: `cd yi-agent-rs && cargo fmt --all && just fmt-check`
Expected: 无 diff,无错误

**Step 2: 跑 lint**

Run: `cd yi-agent-rs && cargo clippy --all-targets --all-features -- -D warnings`
Expected: 无警告

**Step 3: 跑 mock 测试确认无回归**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools --lib && cargo test -p yi-agent-llm --lib && cargo test -p yi-agent-core --lib`
Expected: tools 149 passed, llm 37 passed, core 137 passed 1 failed(预存)

**Step 4: 跑 e2e_real 非 ignored 确认无回归**

Run: `cd yi-agent-rs && cargo test -p yi-agent --test e2e_real`
Expected: `0 passed; 0 failed; 5 ignored`

**Step 5: 跑 e2e_complex 非 ignored 确认无回归**

Run: `cd yi-agent-rs && cargo test -p yi-agent --test e2e_complex`
Expected: `0 passed; 0 failed; 4 ignored`

**Step 6: 验证 justfile recipe**

Run: `cd yi-agent-rs && just test-real-complex`
Expected: `skip: no API key set (.env)`,exit 0

---

## 执行完毕后

完成所有 Task 后,使用 `superpowers:finishing-a-development-branch` skill 合并回 main:

```bash
# 在 main 分支
git merge --no-ff feat/graded-test-system
git branch -d feat/graded-test-system
git worktree remove .worktrees/graded-test-system
```
