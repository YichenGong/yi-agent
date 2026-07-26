//! End-to-end tests calling `yi-agent run` with a real LLM.
//! All tests are #[ignore]'d; run with: cargo test -p yi-agent --test e2e_real -- --ignored
//!
//! 配置源:父进程的环境变量(由 justfile recipe 从 .env 加载,或手动 export)。
//! 测试不硬编码 provider/key,而是透传父进程 env 给子进程,
//! 让 config 层按 YI_AGENT_PROVIDER / MODEL_API_KEY / ANTHROPIC_API_KEY 等解析。

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

/// 检查是否有任何可用的 API key 配置(provider 层或 config 层)。
/// 有 key 才跑真实测试;无 key 则 skip。
fn has_api_key() -> bool {
    let has_provider_key = std::env::var("ANTHROPIC_API_KEY")
        .or_else(|_| std::env::var("OPENAI_API_KEY"))
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    let has_config_key = std::env::var("MODEL_API_KEY")
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    has_provider_key || has_config_key
}

/// 从一行 JSONL 提取 AgentEvent 的 variant 名。
/// serde 对 unit variant(如 Start, Cancelled)序列化为裸字符串 `"Start"`,
/// 对其他 variant 序列化为 `{"VariantName": ...}` 对象。
fn event_variant(line: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if let Some(s) = v.as_str() {
        // unit variant: "Start" / "Cancelled"
        return Some(s.to_string());
    }
    if let Some(obj) = v.as_object() {
        return obj.keys().next().cloned();
    }
    None
}

#[test]
#[ignore]
fn e2e_error_no_api_key() {
    // 无 API key 时 yi-agent run 应以非零退出码失败,stderr 含错误信息。
    // 用空 tempdir 作 workdir,避免加载 ~/.yi-agent/.env 或 ./.yi-agent/.env。
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let output = Command::new(yi_agent_bin())
        .arg("run")
        .arg("--workdir")
        .arg(tmp.path())
        .arg("hi")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("MODEL_API_KEY")
        .env_remove("MODEL_API_URL")
        .env_remove("YI_AGENT_PROVIDER")
        .env_remove("YI_AGENT_MODEL")
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
    if !has_api_key() {
        eprintln!("skip: no API key");
        return;
    }
    // 透传父进程 env(含从 .env 加载的 MODEL_API_KEY / YI_AGENT_PROVIDER 等)。
    let output = Command::new(yi_agent_bin())
        .arg("run")
        .arg("--json")
        .arg("Reply with exactly: hello world")
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
        let ty = event_variant(line).unwrap_or_else(|| panic!("invalid JSONL line: {line}"));
        match ty.as_str() {
            "AssistantText" => {
                let v: serde_json::Value = serde_json::from_str(line).unwrap();
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
    if !has_api_key() {
        eprintln!("skip: no API key");
        return;
    }

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
        let ty = event_variant(line).unwrap_or_else(|| panic!("invalid JSONL: {line}"));
        match ty.as_str() {
            "ToolCall" => {
                let v: serde_json::Value = serde_json::from_str(line).unwrap();
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
    if !has_api_key() {
        eprintln!("skip: no API key");
        return;
    }

    let tmp = tempfile::TempDir::new().expect("tempdir");
    let output = Command::new(yi_agent_bin())
        .arg("--workdir")
        .arg(tmp.path())
        .arg("run")
        .arg("--json")
        .arg("Run the bash command `echo hello` and tell me the output.")
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
        let ty = event_variant(line).unwrap_or_else(|| panic!("invalid JSONL: {line}"));
        match ty.as_str() {
            "ToolCall" => {
                let v: serde_json::Value = serde_json::from_str(line).unwrap();
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
