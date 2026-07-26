//! 共享 helper:供 e2e_real.rs 和 e2e_complex.rs 复用。
//!
//! 每个测试二进制独立编译本模块,未被该二进制使用的 helper 会触发 dead_code。
//! 加 module-level allow 避免每个函数单独标注。

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

/// 复杂测试超时上限(秒)。agent 挂起时强制 kill,避免测试无限阻塞。
const COMPLEX_TIMEOUT: Duration = Duration::from_secs(300);

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

/// 用 `--workdir` + `--json` 启动 yi-agent,超时强制 kill。
///
/// 复杂任务可能因模型死循环或 API 挂起而无限阻塞。用 spawn + 计时线程:
/// 等待 COMPLEX_TIMEOUT 后 kill -9 子进程(已退出则 no-op)。
/// 返回子进程的 Output。killer 线程在测试退出后自然超时结束(不阻塞)。
pub fn run_agent_with_timeout(workdir: &Path, prompt: &str) -> Output {
    let child = Command::new(yi_agent_bin())
        .arg("--workdir")
        .arg(workdir)
        .arg("run")
        .arg("--json")
        .arg(prompt)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn yi-agent");

    let child_id = child.id();

    // 计时线程:超时后 kill 子进程(已退出则 no-op)
    std::thread::spawn(move || {
        std::thread::sleep(COMPLEX_TIMEOUT);
        let _ = std::process::Command::new("kill")
            .arg("-9")
            .arg(child_id.to_string())
            .output();
    });

    child
        .wait_with_output()
        .expect("failed to wait for yi-agent")
}
