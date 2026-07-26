//! End-to-end tests calling `yi-agent run` with a real LLM.
//! All tests are #[ignore]'d; run with: cargo test -p yi-agent --test e2e_real -- --ignored

use std::path::PathBuf;
use std::process::Command;

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

#[test]
#[ignore]
fn e2e_simple_text_response() {
    // Smoke test: real LLM returns text, JSONL contains AssistantText + Done.
    let api_key =
        match std::env::var("ANTHROPIC_API_KEY").or_else(|_| std::env::var("MODEL_API_KEY")) {
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

    assert!(
        output.status.success(),
        "yi-agent run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut found_text = false;
    let mut found_done = false;
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("invalid JSONL line: {line}\nerror: {e}"));
        // AgentEvent is externally-tagged; the variant name is the top-level key.
        let ty = v
            .as_object()
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
            _ => {} // Start, Usage, EstimatedPrefill, etc. are fine
        }
    }
    assert!(
        found_text,
        "should have AssistantText event, stdout: {stdout}"
    );
    assert!(found_done, "should have Done event, stdout: {stdout}");
}

#[test]
#[ignore]
fn e2e_tool_use_read() {
    // Real LLM uses read tool to read a file, then responds.
    let api_key =
        match std::env::var("ANTHROPIC_API_KEY").or_else(|_| std::env::var("MODEL_API_KEY")) {
            Ok(k) => k,
            Err(_) => {
                eprintln!("skip: no API key");
                return;
            }
        };

    let tmp = tempfile::TempDir::new().expect("tempdir");
    let file_path = tmp.path().join("hello.txt");
    std::fs::write(&file_path, "secret123").expect("write");

    let output = Command::new(yi_agent_bin())
        .arg("run")
        .arg("--json")
        .arg(format!(
            "Read the file at {} and tell me its contents.",
            file_path.display()
        ))
        .env("ANTHROPIC_API_KEY", api_key)
        .output()
        .expect("failed to spawn");

    assert!(
        output.status.success(),
        "failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut found_tool_call = false;
    let mut found_tool_result = false;
    let mut found_done = false;
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let v: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("invalid JSONL: {line}\n{e}"));
        let ty = v
            .as_object()
            .and_then(|m| m.keys().next().cloned())
            .unwrap();
        match ty.as_str() {
            "ToolCall" => {
                let name = v["ToolCall"]["name"].as_str().unwrap_or("");
                if name == "read" {
                    found_tool_call = true;
                }
            }
            "ToolResult" => {
                found_tool_result = true;
            }
            "Done" => {
                found_done = true;
            }
            _ => {}
        }
    }
    assert!(found_tool_call, "should call read tool, stdout: {stdout}");
    assert!(
        found_tool_result,
        "should have tool result, stdout: {stdout}"
    );
    assert!(found_done, "should have Done event, stdout: {stdout}");
}

#[test]
#[ignore]
fn e2e_tool_use_bash() {
    // Real LLM uses bash tool to run a command, then responds.
    let api_key =
        match std::env::var("ANTHROPIC_API_KEY").or_else(|_| std::env::var("MODEL_API_KEY")) {
            Ok(k) => k,
            Err(_) => {
                eprintln!("skip: no API key");
                return;
            }
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

    assert!(
        output.status.success(),
        "failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut found_bash_call = false;
    let mut found_done = false;
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let v: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("invalid JSONL: {line}\n{e}"));
        let ty = v
            .as_object()
            .and_then(|m| m.keys().next().cloned())
            .unwrap();
        match ty.as_str() {
            "ToolCall" => {
                if v["ToolCall"]["name"].as_str() == Some("bash") {
                    found_bash_call = true;
                }
            }
            "Done" => {
                found_done = true;
            }
            _ => {}
        }
    }
    assert!(found_bash_call, "should call bash tool, stdout: {stdout}");
    assert!(found_done, "should have Done event, stdout: {stdout}");
}
