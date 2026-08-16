# 真实 LLM 测试系统 Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 为 yi-agent 添加真实 LLM 测试能力,覆盖 provider 层冒烟和 agent loop 端到端,通过新增 `yi-agent run` 非交互式 CLI 子命令作为端到端测试载体。

**Architecture:** 三层实现:(1) `yi-agent run` 子命令复用现有 config/permission setup,绕过 TUI 直接 drain `AgentEvent` 流到 stdout/stderr,支持 `--json` JSONL 输出;(2) provider 层 `#[ignore]` 测试直接调真实 API;(3) 端到端 `#[ignore]` 测试通过 `std::process::Command` 调 `yi-agent run --json` 断言 JSONL。justfile recipe 串联,无 key 时 exit 0 跳过。

**Tech Stack:** Rust, tokio, clap, serde_json, wiremock (现有), tempfile, std::process::Command

**设计文档:** `docs/plans/2026-07-26-real-llm-testing-design.md`

---

## Task 1: 重新应用 CLAUDE.md 与设计文档到 worktree

**Files:**
- 已完成: `.worktrees/real-llm-testing/CLAUDE.md` (已加 "真实 LLM 测试" 小节)
- 已完成: `.worktrees/real-llm-testing/docs/plans/2026-07-26-real-llm-testing-design.md`

**Step 1: Verify files exist in worktree**

Run:
```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/real-llm-testing
grep -c "真实 LLM" CLAUDE.md
ls docs/plans/2026-07-26-real-llm-testing-design.md
```
Expected: `4` and file exists

**Step 2: Commit design docs**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/real-llm-testing
git add CLAUDE.md docs/plans/2026-07-26-real-llm-testing-design.md
git commit -m "docs: add real LLM testing design and CLAUDE.md section"
```

---

## Task 2: 添加 `yi-agent run` CLI 子命令定义

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/config.rs:90-102` (扩展 `Command` enum)

**Step 1: Read current Command enum**

Run: `cargo test -p yi-agent --no-run` (baseline compile check)
Expected: compiles clean

**Step 2: Add Run variant to Command enum**

Modify `yi-agent-rs/crates/yi-agent/src/config.rs` — replace the `Command` enum (lines 90-102):

```rust
/// 子命令
#[derive(clap::Subcommand, Debug)]
pub enum Command {
    /// Run a prompt non-interactively and exit (headless mode).
    Run {
        /// Prompt text. If omitted, reads from stdin.
        prompt: Option<String>,

        /// Output events as JSONL (one AgentEvent per line).
        #[arg(long)]
        json: bool,

        /// Read prompt from stdin even if prompt arg is given.
        #[arg(long)]
        stdin: bool,
    },
    /// Start web config UI
    Web {
        /// Host to bind
        #[arg(long, default_value = "127.0.0.1")]
        host: String,

        /// Port to bind
        #[arg(long, default_value = "7292")]
        port: u16,
    },
}
```

**Step 3: Verify compile**

Run: `cargo build -p yi-agent`
Expected: compiles (will warn about unused `Run` variant — that's fine, wired next)

**Step 4: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/src/config.rs
git commit -m "feat: add `yi-agent run` subcommand CLI definition"
```

---

## Task 3: 实现 `run_headless()` 函数 — 人读模式

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs` (新增 `run_headless` + 分支)

**Step 1: Add Command::Run branch in main()**

Modify `yi-agent-rs/crates/yi-agent/src/main.rs` — replace the `match cli.command` block (lines 21-36):

```rust
    match cli.command {
        Some(Command::Web { ref host, ref port }) => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async {
                let env_path = config::resolve_env_path(&cli);
                let global_env_path = if config::is_workdir_explicit(&cli) {
                    None
                } else {
                    config::resolve_global_env_path()
                };
                yi_agent_web::serve(host, *port, env_path, global_env_path).await
            })
        }
        Some(Command::Run {
            ref prompt,
            json,
            stdin,
        }) => run_headless(cli, prompt.clone(), json, stdin),
        None => run_agent(cli),
    }
```

**Step 2: Add run_headless function**

Append to `yi-agent-rs/crates/yi-agent/src/main.rs` (before the `run_tui_agent` function, after `run_agent`):

```rust
/// Run agent non-interactively: drain AgentEvent stream to stdout/stderr.
/// Used for headless CLI usage and end-to-end real-LLM testing.
fn run_headless(cli: Cli, prompt: Option<String>, json: bool, from_stdin: bool) -> Result<()> {
    use futures::StreamExt;

    let config = config::load(&cli)?;

    // Resolve prompt: explicit stdin flag > no prompt arg > prompt arg
    let prompt_text = if from_stdin || prompt.is_none() {
        let mut buf = String::new();
        std::io::stdin().read_line(&mut buf)?;
        buf.trim_end_matches('\n').to_string()
    } else {
        prompt.unwrap()
    };
    if prompt_text.is_empty() {
        anyhow::bail!("empty prompt");
    }

    let workdir = config.workdir.clone();
    // Headless mode: auto-allow non-blacklisted tools (yolo behavior)
    let yolo = true;
    let permissions = {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(yi_agent_core::permission::PermissionChecker::load(&workdir))
            .map_err(|e| anyhow::anyhow!("failed to load permissions: {e}"))?
    };
    let blocklist_fn: yi_agent_core::permission::BlocklistFn =
        Arc::new(|cmd: &str| yi_agent_tools::blocklist::is_blocked(cmd).map(|s| s.to_string()));
    let checker = Arc::new(yi_agent_core::permission::PermissionChecker::new(
        permissions,
        yolo,
        workdir.clone(),
        blocklist_fn,
    ));
    let (_decision_tx, decision_rx) =
        tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);

    let provider: Arc<dyn yi_agent_core::Provider> = match config.provider.as_str() {
        "anthropic" => Arc::new(yi_agent_llm::AnthropicProvider::new(
            yi_agent_llm::AnthropicProviderOpts {
                base_url: Some(config.api_url.clone()),
                api_key: Some(config.api_key.clone()),
                ..Default::default()
            },
        )?),
        "openai" => Arc::new(yi_agent_llm::OpenaiProvider::new(
            yi_agent_llm::OpenaiProviderOpts {
                base_url: Some(config.api_url.clone()),
                api_key: Some(config.api_key.clone()),
                ..Default::default()
            },
        )?),
        other => anyhow::bail!(
            "unknown provider '{}': expected 'anthropic' or 'openai'",
            other
        ),
    };

    let mut registry = yi_agent_core::ToolRegistry::new();
    yi_agent_tools::register_builtin_tools(&mut registry, config.workdir.clone());
    let tools = Arc::new(registry);

    let agent_config = yi_agent_core::AgentConfig {
        model: config.model.clone(),
        system_prompt: config.system_prompt.clone(),
        max_turns: Some(config.max_turns),
        ..Default::default()
    };

    let rt = tokio::runtime::Runtime::new()?;
    let exit_code = rt.block_on(async move {
        let decision_rx = Arc::new(tokio::sync::Mutex::new(decision_rx));
        let agent = yi_agent_core::Agent::new(provider, tools, agent_config)
            .with_permission(checker, decision_rx);

        let stream = match agent.run(prompt_text).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: {e}");
                return 1;
            }
        };

        let mut stream = Box::pin(stream);
        let mut exit_code = 0;
        while let Some(event) = stream.next().await {
            if json {
                let line = serde_json::to_string(&event).unwrap_or_else(|_| "{}".into());
                println!("{line}");
            } else {
                match &event {
                    yi_agent_core::AgentEvent::AssistantText(t) => {
                        println!("{t}");
                    }
                    yi_agent_core::AgentEvent::ToolCall { name, input, .. } => {
                        eprintln!("[tool:{name}] {input}");
                    }
                    yi_agent_core::AgentEvent::ToolResult { id, result } => {
                        eprintln!("[result:{id}] error={} content={:?}",
                            result.is_error, result.content);
                    }
                    yi_agent_core::AgentEvent::Done { reason } => {
                        eprintln!("[done:{reason:?}]");
                    }
                    yi_agent_core::AgentEvent::Cancelled => {
                        eprintln!("[cancelled]");
                        exit_code = 130;
                    }
                    yi_agent_core::AgentEvent::Error(e) => {
                        eprintln!("[error:{e}]");
                        exit_code = 1;
                    }
                    _ => {}  // Usage, EstimatedPrefill, DecodeDelta, ToolOutputDelta, etc.
                }
            }
            // Exit on terminal events
            if matches!(event,
                yi_agent_core::AgentEvent::Done { .. }
                | yi_agent_core::AgentEvent::Cancelled
                | yi_agent_core::AgentEvent::Error(_)
            ) {
                break;
            }
        }
        exit_code
    });

    std::process::exit(exit_code);
}
```

**Step 3: Verify compile**

Run: `cargo build -p yi-agent`
Expected: compiles clean (may need `use std::io::BufRead;` if not already imported)

**Step 4: Test manually (no API key needed for compile/path)**

Run:
```bash
cargo run -p yi-agent -- run "hi" 2>&1 | head -5
```
Expected: error about missing API key (proves CLI path works end-to-end up to provider construction)

**Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/src/main.rs
git commit -m "feat: implement `yi-agent run` headless mode"
```

---

## Task 4: 添加 `AgentEvent` 的 Serialize derive

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs:114-164` (`AgentEvent` enum)
- Modify: `yi-agent-rs/crates/yi-agent-core/Cargo.toml` (确保 serde feature)

**Step 1: Check current AgentEvent derives**

`AgentEvent` currently has `#[derive(Debug, Clone)]`. Need to add `Serialize` for `--json` mode.

**Step 2: Add serde derive to AgentEvent and its sub-types**

Modify `yi-agent-rs/crates/yi-agent-core/src/agent.rs`:

- Line 115: change `#[derive(Debug, Clone)]` to `#[derive(Debug, Clone, serde::Serialize)]`
- Line 166 `DoneReason`: change `#[derive(Debug, Clone, PartialEq, Eq)]` to `#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]`
- Line 172 `AgentError`: add `serde::Serialize` (check `thiserror` derive still works; may need `#[serde(transparent)]` or manual impl — see Step 3)

**Step 3: Check AgentError serialization**

`AgentError` has `#[error("provider error: {0}")] Provider(#[from] ProviderError)`. Need to verify `ProviderError` also derives `Serialize`. Check:

```bash
grep -n "derive.*Serialize" yi-agent-rs/crates/yi-agent-core/src/provider.rs | head -5
grep -n "pub enum ProviderError" yi-agent-rs/crates/yi-agent-core/src/provider.rs
```

If `ProviderError` lacks `Serialize`, add `serde::Serialize` to it (and all its variants' payloads must also be serializable — check `StopReason`, `TokenUsage`).

Also check `ToolResult` in `tool.rs:36` — add `serde::Serialize` if missing.

Also check `OutputStream` in `tool.rs` — add `serde::Serialize` if missing.

**Step 4: Check Cargo.toml has serde with derive feature**

`yi-agent-rs/crates/yi-agent-core/Cargo.toml` should have:
```toml
serde = { version = "1", features = ["derive"] }
```
If only `serde = "1"`, add `features = ["derive"]`.

**Step 5: Verify compile**

Run: `cargo build -p yi-agent-core`
Expected: compiles clean

**Step 6: Verify JSON output works**

Run:
```bash
cargo run -p yi-agent -- run --json "hi" 2>&1 | head -3
```
Expected: error output (no API key) but no serde panic

**Step 7: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-core/src/agent.rs yi-agent-rs/crates/yi-agent-core/src/provider.rs yi-agent-rs/crates/yi-agent-core/src/tool.rs yi-agent-rs/crates/yi-agent-core/Cargo.toml
git commit -m "feat: add Serialize to AgentEvent for --json output"
```

---

## Task 5: 端到端测试 — 无 API key 错误路径

**Files:**
- Create: `yi-agent-rs/crates/yi-agent/tests/e2e_real.rs`

**Step 1: Create e2e_real.rs with one test (error path)**

Create `yi-agent-rs/crates/yi-agent/tests/e2e_real.rs`:

```rust
//! End-to-end tests calling `yi-agent run` with a real LLM.
//! All tests are #[ignore]'d; run with: cargo test -p yi-agent --test e2e_real -- --ignored

use std::process::Command;
use std::path::PathBuf;

/// Path to the compiled yi-agent binary.
fn yi_agent_bin() -> PathBuf {
    // CARGO_BIN_EXE_yi-agent is set by cargo when running integration tests.
    // Fallback to target/debug/yi-agent for manual runs.
    option_env!("CARGO_BIN_EXE_yi-agent")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/debug/yi-agent"))
}

#[test]
#[ignore]
fn e2e_error_no_api_key() {
    // No API key in env: yi-agent run should fail with non-zero exit and
    // an auth/provider error message on stderr.
    let output = Command::new(yi_agent_bin())
        .arg("run")
        .arg("hi")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("MODEL_API_KEY")
        .output()
        .expect("failed to spawn yi-agent");

    assert!(!output.status.success(), "should fail without API key");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.to_lowercase().contains("error")
            || stderr.to_lowercase().contains("auth")
            || stderr.to_lowercase().contains("api key"),
        "stderr should mention error/auth/api key, got: {stderr}"
    );
}
```

**Step 2: Run the ignored test**

Run:
```bash
cargo test -p yi-agent --test e2e_real -- --ignored e2e_error_no_api_key
```
Expected: PASS (proves binary path resolution + auth error path works)

**Step 3: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/tests/e2e_real.rs
git commit -m "test: add e2e error-path test for yi-agent run"
```

---

## Task 6: 端到端测试 — 简单文本响应(需真实 API key)

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/tests/e2e_real.rs` (追加测试)

**Step 1: Add e2e_simple_text_response test**

Append to `yi-agent-rs/crates/yi-agent/tests/e2e_real.rs`:

```rust
#[test]
#[ignore]
fn e2e_simple_text_response() {
    // Smoke test: real LLM returns text, JSONL contains AssistantText + Done.
    let api_key = match std::env::var("ANTHROPIC_API_KEY").or_else(|_| std::env::var("MODEL_API_KEY")) {
        Ok(k) => k,
        Err(_) => {
            eprintln!("skip: no ANTHROPIC_API_KEY / MODEL_API_KEY");
            return;
        }
    };

    let output = Command::new(yi_agent_bin())
        .arg("run")
        .arg("--json")
        .arg("Reply with exactly: hello world")
        .env("ANTHROPIC_API_KEY", api_key)
        .output()
        .expect("failed to spawn yi-agent");

    assert!(output.status.success(), "yi-agent run failed: {}",
        String::from_utf8_lossy(&output.stderr));

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut found_text = false;
    let mut found_done = false;
    for line in stdout.lines() {
        if line.trim().is_empty() { continue; }
        let v: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("invalid JSONL line: {line}\nerror: {e}"));
        // AgentEvent is internally tagged; check for variants.
        let ty = v.as_object()
            .and_then(|m| m.keys().next().cloned())
            .unwrap_or_else(|| panic!("expected tagged enum, got: {line}"));
        match ty.as_str() {
            "AssistantText" => {
                let text = v["AssistantText"].as_str().unwrap_or("");
                assert!(!text.is_empty(), "assistant text should be non-empty");
                found_text = true;
            }
            "Done" => {
                found_done = true;
            }
            _ => {}  // Start, Usage, EstimatedPrefill, etc. are fine
        }
    }
    assert!(found_text, "should have AssistantText event, stdout: {stdout}");
    assert!(found_done, "should have Done event, stdout: {stdout}");
}
```

**Step 2: Verify it compiles**

Run: `cargo test -p yi-agent --test e2e_real --no-run`
Expected: compiles clean

**Step 3: Run (only if API key available)**

Run:
```bash
ANTHROPIC_API_KEY=sk-... cargo test -p yi-agent --test e2e_real -- --ignored e2e_simple_text_response
```
Expected: PASS (if real key) or skip print (if no key)

**Step 4: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/tests/e2e_real.rs
git commit -m "test: add e2e simple text response test with real LLM"
```

---

## Task 7: 端到端测试 — 工具调用(需真实 API key)

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/tests/e2e_real.rs`

**Step 1: Add tool-use tests**

Append to `yi-agent-rs/crates/yi-agent/tests/e2e_real.rs`:

```rust
#[test]
#[ignore]
fn e2e_tool_use_read() {
    // Real LLM uses read tool to read a file, then responds.
    let api_key = match std::env::var("ANTHROPIC_API_KEY").or_else(|_| std::env::var("MODEL_API_KEY")) {
        Ok(k) => k,
        Err(_) => { eprintln!("skip: no API key"); return; }
    };

    let tmp = tempfile::TempDir::new().expect("tempdir");
    let file_path = tmp.path().join("hello.txt");
    std::fs::write(&file_path, "secret123").expect("write");

    let output = Command::new(yi_agent_bin())
        .arg("run")
        .arg("--json")
        .arg(format!("Read the file at {} and tell me its contents.", file_path.display()))
        .env("ANTHROPIC_API_KEY", api_key)
        .output()
        .expect("failed to spawn");

    assert!(output.status.success(), "failed: {}",
        String::from_utf8_lossy(&output.stderr));

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut found_tool_call = false;
    let mut found_tool_result = false;
    let mut found_done = false;
    for line in stdout.lines() {
        if line.trim().is_empty() { continue; }
        let v: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("invalid JSONL: {line}\n{e}"));
        let ty = v.as_object().and_then(|m| m.keys().next().cloned()).unwrap();
        match ty.as_str() {
            "ToolCall" => {
                let name = v["ToolCall"]["name"].as_str().unwrap_or("");
                if name == "read" { found_tool_call = true; }
            }
            "ToolResult" => { found_tool_result = true; }
            "Done" => { found_done = true; }
            _ => {}
        }
    }
    assert!(found_tool_call, "should call read tool, stdout: {stdout}");
    assert!(found_tool_result, "should have tool result, stdout: {stdout}");
    assert!(found_done, "should have Done event, stdout: {stdout}");
}

#[test]
#[ignore]
fn e2e_tool_use_bash() {
    // Real LLM uses bash tool to run a command, then responds.
    let api_key = match std::env::var("ANTHROPIC_API_KEY").or_else(|_| std::env::var("MODEL_API_KEY")) {
        Ok(k) => k,
        Err(_) => { eprintln!("skip: no API key"); return; }
    };

    let tmp = tempfile::TempDir::new().expect("tempdir");
    let output = Command::new(yi_agent_bin())
        .arg("run")
        .arg("--json")
        .arg("Run the bash command `echo hello` and tell me the output.")
        .arg("--workdir")
        .arg(tmp.path())
        .env("ANTHROPIC_API_KEY", api_key)
        .output()
        .expect("failed to spawn");

    assert!(output.status.success(), "failed: {}",
        String::from_utf8_lossy(&output.stderr));

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut found_bash_call = false;
    let mut found_done = false;
    for line in stdout.lines() {
        if line.trim().is_empty() { continue; }
        let v: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("invalid JSONL: {line}\n{e}"));
        let ty = v.as_object().and_then(|m| m.keys().next().cloned()).unwrap();
        match ty.as_str() {
            "ToolCall" => {
                if v["ToolCall"]["name"].as_str() == Some("bash") {
                    found_bash_call = true;
                }
            }
            "Done" => { found_done = true; }
            _ => {}
        }
    }
    assert!(found_bash_call, "should call bash tool, stdout: {stdout}");
    assert!(found_done, "should have Done event, stdout: {stdout}");
}
```

**Step 2: Add tempfile dev-dependency**

Modify `yi-agent-rs/crates/yi-agent/Cargo.toml` — add to `[dev-dependencies]`:
```toml
tempfile = "3"
serde_json = "1"
```
(serde_json may already be a dep via transitive — check first with `grep serde_json yi-agent-rs/crates/yi-agent/Cargo.toml`)

**Step 3: Verify compile**

Run: `cargo test -p yi-agent --test e2e_real --no-run`
Expected: compiles clean

**Step 4: Run (only if API key available)**

Run:
```bash
ANTHROPIC_API_KEY=sk-... cargo test -p yi-agent --test e2e_real -- --ignored e2e_tool_use_read
ANTHROPIC_API_KEY=sk-... cargo test -p yi-agent --test e2e_real -- --ignored e2e_tool_use_bash
```
Expected: PASS or skip

**Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/tests/e2e_real.rs yi-agent-rs/crates/yi-agent/Cargo.toml
git commit -m "test: add e2e tool-use tests (read + bash) with real LLM"
```

---

## Task 8: Provider 层真实 LLM 冒烟测试

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-llm/tests/real_integration.rs`
- Modify: `yi-agent-rs/crates/yi-agent-llm/Cargo.toml` (确认 dev-deps)

**Step 1: Create real_integration.rs with Anthropic tests**

Create `yi-agent-rs/crates/yi-agent-llm/tests/real_integration.rs`:

```rust
//! Real-API smoke tests for AnthropicProvider and OpenaiProvider.
//! All tests are #[ignore]'d; run with:
//!   cargo test -p yi-agent-llm --test real_integration -- --ignored

use std::time::Duration;
use futures::stream::StreamExt;
use yi_agent_core::{
    ContentBlock, GenParams, Message, Provider, ProviderError, ProviderEvent, ProviderRequest,
    ProviderResponse, StopReason,
};
use yi_agent_llm::{AnthropicProvider, AnthropicProviderOpts};

fn skip_if_no_key(env_var: &str) -> Option<String> {
    match std::env::var(env_var) {
        Ok(k) if !k.is_empty() => Some(k),
        _ => {
            eprintln!("skip: no {env_var}");
            None
        }
    }
}

fn simple_request(model: &str) -> ProviderRequest {
    ProviderRequest {
        model: model.to_string(),
        system: None,
        messages: vec![Message::user("Reply with exactly: hello world")],
        tools: vec![],
        params: GenParams::default(),
    }
}

async fn collect_events(
    stream: futures::stream::BoxStream<'static, ProviderEvent>,
) -> Vec<ProviderEvent> {
    let mut s = stream;
    let mut out = Vec::new();
    while let Some(e) = s.next().await {
        out.push(e);
    }
    out
}

#[tokio::test]
#[ignore]
async fn real_anthropic_text_stream() {
    let key = match skip_if_no_key("ANTHROPIC_API_KEY") {
        Some(k) => k,
        None => return,
    };
    let provider = AnthropicProvider::new(AnthropicProviderOpts {
        api_key: Some(key),
        timeout: Some(Duration::from_secs(30)),
        ..Default::default()
    })
    .expect("provider construction");

    let stream = provider
        .call_stream(simple_request("claude-sonnet-4-5"))
        .await
        .expect("stream ok");
    let events = collect_events(stream).await;

    let text: String = events
        .iter()
        .filter_map(|e| {
            if let ProviderEvent::TextDelta(t) = e {
                Some(t.clone())
            } else {
                None
            }
        })
        .collect();
    assert!(!text.is_empty(), "should have text, events: {events:?}");
    assert!(
        events.iter().any(|e| matches!(e, ProviderEvent::Stop { .. })),
        "should have Stop event"
    );
}

#[tokio::test]
#[ignore]
async fn real_anthropic_tool_use() {
    let key = match skip_if_no_key("ANTHROPIC_API_KEY") {
        Some(k) => k,
        None => return,
    };
    let provider = AnthropicProvider::new(AnthropicProviderOpts {
        api_key: Some(key),
        timeout: Some(Duration::from_secs(30)),
        ..Default::default()
    })
    .expect("provider");

    let req = ProviderRequest {
        model: "claude-sonnet-4-5".to_string(),
        system: None,
        messages: vec![Message::user("What is 2+2? Use the calculator tool.")],
        tools: vec![yi_agent_core::ToolSchema {
            name: "calculator".to_string(),
            description: "Basic calculator".to_string(),
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "expr": {"type": "string"}
                },
                "required": ["expr"]
            }),
        }],
        params: GenParams::default(),
    };

    let stream = provider.call_stream(req).await.expect("stream ok");
    let events = collect_events(stream).await;

    assert!(
        events.iter().any(|e| matches!(e, ProviderEvent::ToolUseStart { .. })),
        "should have ToolUseStart, events: {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(e, ProviderEvent::ToolUseEnd { .. })),
        "should have ToolUseEnd"
    );
}

#[tokio::test]
#[ignore]
async fn real_anthropic_call_accumulate() {
    let key = match skip_if_no_key("ANTHROPIC_API_KEY") {
        Some(k) => k,
        None => return,
    };
    let provider = AnthropicProvider::new(AnthropicProviderOpts {
        api_key: Some(key),
        timeout: Some(Duration::from_secs(30)),
        ..Default::default()
    })
    .expect("provider");

    let resp: ProviderResponse = provider
        .call(simple_request("claude-sonnet-4-5"))
        .await
        .expect("call ok");

    assert!(!resp.content.is_empty(), "should have content");
    assert!(
        resp.content.iter().any(|b| matches!(b, ContentBlock::Text(_))),
        "should have text block, got: {:?}",
        resp.content
    );
}

#[tokio::test]
#[ignore]
async fn real_anthropic_env_auth() {
    // No key: provider construction should fail with Auth error.
    unsafe {
        std::env::remove_var("ANTHROPIC_API_KEY");
    }
    let result = AnthropicProvider::new(AnthropicProviderOpts {
        api_key: None,
        ..Default::default()
    });
    assert!(
        matches!(result, Err(ProviderError::Auth(_))),
        "expected Auth error, got: {result:?}"
    );
}
```

**Step 2: Check ToolSchema field names**

The test above uses `yi_agent_core::ToolSchema { name, description, schema }`. Verify these field names match the actual struct:

```bash
grep -A 10 "pub struct ToolSchema" yi-agent-rs/crates/yi-agent-core/src/tool.rs
```
If field names differ, fix the test to match.

**Step 3: Verify compile**

Run: `cargo test -p yi-agent-llm --test real_integration --no-run`
Expected: compiles clean

**Step 4: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-llm/tests/real_integration.rs
git commit -m "test: add real-API smoke tests for AnthropicProvider"
```

---

## Task 9: OpenAI provider 真实测试

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-llm/tests/real_integration.rs`

**Step 1: Append OpenAI tests**

Append to `yi-agent-rs/crates/yi-agent-llm/tests/real_integration.rs`:

```rust
// === OpenAI provider real-API tests ===

use yi_agent_llm::{OpenaiProvider, OpenaiProviderOpts};

#[tokio::test]
#[ignore]
async fn real_openai_text_stream() {
    let key = match skip_if_no_key("OPENAI_API_KEY") {
        Some(k) => k,
        None => return,
    };
    let provider = OpenaiProvider::new(OpenaiProviderOpts {
        api_key: Some(key),
        timeout: Some(Duration::from_secs(30)),
        ..Default::default()
    })
    .expect("provider");

    let stream = provider
        .call_stream(simple_request("gpt-4o"))
        .await
        .expect("stream ok");
    let events = collect_events(stream).await;

    let text: String = events
        .iter()
        .filter_map(|e| {
            if let ProviderEvent::TextDelta(t) = e {
                Some(t.clone())
            } else {
                None
            }
        })
        .collect();
    assert!(!text.is_empty(), "should have text");
    assert!(events.iter().any(|e| matches!(e, ProviderEvent::Stop { .. })));
}

#[tokio::test]
#[ignore]
async fn real_openai_call_accumulate() {
    let key = match skip_if_no_key("OPENAI_API_KEY") {
        Some(k) => k,
        None => return,
    };
    let provider = OpenaiProvider::new(OpenaiProviderOpts {
        api_key: Some(key),
        timeout: Some(Duration::from_secs(30)),
        ..Default::default()
    })
    .expect("provider");

    let resp: ProviderResponse = provider
        .call(simple_request("gpt-4o"))
        .await
        .expect("call ok");

    assert!(!resp.content.is_empty());
    assert!(resp.content.iter().any(|b| matches!(b, ContentBlock::Text(_))));
}

#[tokio::test]
#[ignore]
async fn real_openai_env_auth() {
    unsafe {
        std::env::remove_var("OPENAI_API_KEY");
    }
    let result = OpenaiProvider::new(OpenaiProviderOpts {
        api_key: None,
        ..Default::default()
    });
    assert!(matches!(result, Err(ProviderError::Auth(_))));
}
```

**Step 2: Verify compile**

Run: `cargo test -p yi-agent-llm --test real_integration --no-run`
Expected: compiles clean

**Step 3: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-llm/tests/real_integration.rs
git commit -m "test: add real-API smoke tests for OpenaiProvider"
```

---

## Task 10: justfile recipe

**Files:**
- Modify: `yi-agent-rs/justfile` (追加 recipe)

**Step 1: Append real-LLM recipes**

Append to `yi-agent-rs/justfile` (after the `test` recipe around line 23):

```makefile

# === 真实 LLM 测试(需 API key,默认跳过) ===

# 跑真实 LLM provider 层测试
test-real-llm:
    #!/usr/bin/env bash
    set -e
    if [ -z "$ANTHROPIC_API_KEY" ] && [ -z "$OPENAI_API_KEY" ] && [ -z "$MODEL_API_KEY" ]; then
        echo "skip: no API key set (ANTHROPIC_API_KEY / OPENAI_API_KEY / MODEL_API_KEY)"
        exit 0
    fi
    cargo test -p yi-agent-llm --test real_integration -- --ignored

# 跑真实 LLM 端到端测试(经 `yi-agent run`)
test-real-e2e:
    #!/usr/bin/env bash
    set -e
    if [ -z "$ANTHROPIC_API_KEY" ] && [ -z "$OPENAI_API_KEY" ] && [ -z "$MODEL_API_KEY" ]; then
        echo "skip: no API key set"
        exit 0
    fi
    cargo test -p yi-agent --test e2e_real -- --ignored

# 跑所有真实 LLM 测试
test-real-all: test-real-llm test-real-e2e
    @echo "Real LLM tests passed"
```

**Step 2: Verify justfile parses**

Run: `just --list`
Expected: lists `test-real-llm`, `test-real-e2e`, `test-real-all` among recipes

**Step 3: Verify skip-when-no-key works**

Run:
```bash
unset ANTHROPIC_API_KEY OPENAI_API_KEY MODEL_API_KEY
just test-real-llm
```
Expected: prints "skip: no API key set" and exits 0

**Step 4: Commit**

```bash
git add yi-agent-rs/justfile
git commit -m "ci: add justfile recipes for real LLM tests"
```

---

## Task 11: 验证默认 cargo test 不跑真实测试

**Files:** 无修改,仅验证

**Step 1: Run default cargo test (no --ignored)**

Run:
```bash
ps aux | grep -v grep | grep -E "cargo|rustc|yi_agent" | head -3  # 确认无残留
cargo test -p yi-agent-llm
cargo test -p yi-agent
```
Expected:
- `cargo test -p yi-agent-llm`: 13 tests pass, 4 ignored (real_integration)
- `cargo test -p yi-agent`: 203+ tests pass, 4 ignored (e2e_real)

**Step 2: Verify with --list**

Run:
```bash
cargo test -p yi-agent-llm -- --list 2>&1 | grep -E "real_|e2e_"
cargo test -p yi-agent -- --list 2>&1 | grep -E "real_|e2e_"
```
Expected: shows ignored tests

**Step 3: Run fmt + clippy (CI parity)**

Run:
```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/real-llm-testing/yi-agent-rs
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -10
```
Expected: no warnings

**Step 4: Commit any fmt fixes**

```bash
git add -A
git commit -m "style: cargo fmt"  # only if changes
```

---

## Task 12: 最终验证 + 清理

**Step 1: Run full CI locally**

Run:
```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/real-llm-testing/yi-agent-rs
just ci
```
Expected: all pass (fmt-check, lint, test, build)

**Step 2: Verify real-LLM skip path**

Run:
```bash
just test-real-all
```
Expected (no key): "skip: no API key set" x2, exit 0

**Step 3: Final commit status**

Run:
```bash
git status --short
git log --oneline -10
```
Expected: clean working tree, all commits present

**Step 4: Merge to main (per finishing-a-development-branch skill)**

Use superpowers:finishing-a-development-branch skill to decide merge/PR strategy.

---

## 注意事项

- **每个 Task 后跑 `cargo fmt --all`** 保证格式
- **每个 Task 后跑 `cargo build -p <crate>` 或 `cargo test -p <crate> --no-run`** 验证编译
- **不要同时跑多个 cargo 命令**(CLAUDE.md cargo test 执行章节)
- **真实 API 测试**只在 Task 6/7/8/9 的 Step 3/4 跑,且需 API key;无 key 时自动 skip
- **AgentEvent Serialize** 在 Task 4 可能需要级联给 `ProviderError`/`ToolResult`/`OutputStream` 加 Serialize,按编译错误驱动修改
- **ToolSchema 字段名** Task 8 需要先用 grep 确认实际字段名
