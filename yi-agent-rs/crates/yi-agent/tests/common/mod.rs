//! 共享 helper:供 e2e_real.rs 和 e2e_complex.rs 复用。
//!
//! 每个测试二进制独立编译本模块,未被该二进制使用的 helper 会触发 dead_code。
//! 加 module-level allow 避免每个函数单独标注。

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::time::Duration;

/// 复杂测试超时上限(秒)。agent 挂起时强制 kill,避免测试无限阻塞。
const COMPLEX_TIMEOUT: Duration = Duration::from_secs(300);

/// Path to the compiled yi-agent binary.
pub fn yi_agent_bin() -> PathBuf {
    option_env!("CARGO_BIN_EXE_yi-agent")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/debug/yi-agent"))
}

/// 检查 headless CLI 所需的 API key 配置。
pub fn has_api_key() -> bool {
    std::env::var("MODEL_API_KEY")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
}

/// 无 key 时打印 skip 并返回 false。
pub fn skip_if_no_key() -> bool {
    if !has_api_key() {
        eprintln!("SKIPPED: no headless configuration (MODEL_API_KEY)");
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
}

/// 从一行 JSONL 提取 AgentEvent 的 variant 名。
/// serde 对 unit variant(如 Start, Cancelled)序列化为裸字符串 `"Start"`,
/// 对其他 variant 序列化为 `{"VariantName": ...}` 对象。
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
            || v.as_object()
                .map(|o| o.contains_key("Done"))
                .unwrap_or(false)
    })
}

pub fn has_normal_end_turn(events: &[serde_json::Value]) -> bool {
    events
        .iter()
        .any(|event| event.pointer("/Done/reason") == Some(&serde_json::json!("EndTurn")))
}

pub fn has_verification_after_last_mutation(events: &[serde_json::Value]) -> bool {
    let mut seen_mutation = false;
    let mut verified = false;
    for event in events {
        let Some(name) = event
            .pointer("/ToolCall/name")
            .and_then(serde_json::Value::as_str)
        else {
            continue;
        };
        if matches!(name, "write" | "edit" | "bash") {
            seen_mutation = true;
            verified = false;
        } else if seen_mutation && matches!(name, "read" | "glob" | "grep") {
            verified = true;
        }
    }
    seen_mutation && verified
}

fn wait_for_child(mut child: Child, timeout: Duration) -> Result<Output, String> {
    let started = std::time::Instant::now();

    loop {
        if child
            .try_wait()
            .map_err(|err| format!("failed to poll yi-agent: {err}"))?
            .is_some()
        {
            return child
                .wait_with_output()
                .map_err(|err| format!("failed to collect yi-agent output: {err}"));
        }

        if started.elapsed() >= timeout {
            child
                .kill()
                .map_err(|err| format!("failed to terminate timed-out yi-agent: {err}"))?;
            let output = child
                .wait_with_output()
                .map_err(|err| format!("failed to collect timed-out yi-agent output: {err}"))?;
            return Err(format!(
                "yi-agent timed out after {}s: {}",
                timeout.as_secs(),
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        std::thread::sleep(Duration::from_millis(10));
    }
}

pub fn run_command_with_timeout(
    command: &mut Command,
    timeout: Duration,
) -> Result<Output, String> {
    let child = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|err| format!("failed to spawn yi-agent: {err}"))?;
    wait_for_child(child, timeout)
}

/// 用 `--workdir` + `--json` 启动 yi-agent,超时后仅终止自己持有的 Child。
pub fn run_agent_with_timeout(workdir: &Path, prompt: &str) -> Result<Output, String> {
    let mut command = Command::new(yi_agent_bin());
    command
        .arg("--workdir")
        .arg(workdir)
        .arg("run")
        .arg("--json")
        .arg(prompt);
    run_command_with_timeout(&mut command, COMPLEX_TIMEOUT)
}

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

    #[test]
    fn completion_helpers_require_normal_end_and_post_write_verification() {
        let events = parse_events(
            r#"{"ToolCall":{"name":"write"}}
{"ToolCall":{"name":"read"}}
{"Done":{"reason":"EndTurn"}}"#,
        );
        assert!(has_normal_end_turn(&events));
        assert!(has_verification_after_last_mutation(&events));
    }
}
