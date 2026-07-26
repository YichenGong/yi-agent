//! 共享 helper:供 e2e_real.rs 和 e2e_complex.rs 复用。

use std::path::PathBuf;

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
