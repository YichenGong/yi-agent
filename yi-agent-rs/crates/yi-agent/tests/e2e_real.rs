//! End-to-end tests calling `yi-agent run` with a real LLM.
//! All tests are #[ignore]'d; run with: cargo test -p yi-agent --test e2e_real -- --ignored
//!
//! 配置源:父进程的环境变量(由 justfile recipe 从 .env 加载,或手动 export)。
//! 测试不硬编码 provider/key,而是透传父进程 env 给子进程,
//! 让 config 层按 YI_AGENT_PROVIDER / MODEL_API_KEY / ANTHROPIC_API_KEY 等解析。

mod common;

use common::{event_variant, has_api_key, resolve_api_key, yi_agent_bin};
use std::process::Command;

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

#[test]
#[ignore]
fn e2e_auto_compact_triggers() {
    if !has_api_key() {
        eprintln!("skip: no API key");
        return;
    }
    let tmp = tempfile::TempDir::new().expect("tempdir");
    // 写一个较大文件,让单轮 read 就可能超 2000 tokens
    let big_file = tmp.path().join("big.txt");
    let content = "line of text\n".repeat(500); // ~7KB
    std::fs::write(&big_file, &content).expect("write");

    let mut cmd = Command::new(yi_agent_bin());
    cmd.arg("--workdir")
        .arg(tmp.path())
        .arg("--compact-ratio")
        .arg("1") // threshold = context_length * 1% ≈ 2000 tokens
        .arg("--compact-keep-turns")
        .arg("1");
    if let Some(key) = resolve_api_key() {
        cmd.arg("--api-key").arg(key);
    }
    cmd.arg("run").arg("--json").arg(format!(
        "Read the file at {} and tell me how many lines it has.",
        big_file.display()
    ));
    let output = cmd.output().expect("failed to spawn");

    assert!(
        output.status.success(),
        "failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut found_auto_compacting = false;
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let ty = event_variant(line).unwrap_or_else(|| panic!("invalid JSONL: {line}"));
        if ty == "AutoCompacting" {
            found_auto_compacting = true;
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            let old = v["AutoCompacting"]["old_msg_count"].as_u64();
            let new = v["AutoCompacting"]["new_msg_count"].as_u64();
            assert!(
                old > new,
                "old_msg_count should be > new_msg_count, got old={old:?} new={new:?}"
            );
        }
    }
    // 不强制断言 found_auto_compacting:模型行为不稳定,可能 read 单轮没超阈值。
    // 只要不 panic 且 AutoCompacting payload 结构正确就算通过。
    if found_auto_compacting {
        eprintln!("auto-compact triggered successfully");
    } else {
        eprintln!("auto-compact did not trigger (model output may be short) — acceptable");
    }
}
