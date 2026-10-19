# 模型级 reasoning effort 能力探测 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 给 `~/.yi-agent/models.json` 的每个模型条目记录「该模型在该通道上支持哪些 reasoning effort 档位」，并让三端 UI 只展示支持的档位。

**Architecture:** 探测走 `yi-agent-llm` 新增的**裸 HTTP** 路径（刻意绕开 `Provider::call_stream` 的「被拒剥离重投」与「responses 缺失回退 chat」兜底，否则会把不支持伪装成成功）。编排放在 `yi-agent-runtime`（基线 + 7 档串行、预算、写回防覆盖）。app-server 负责 `model/probe` RPC、`model/upsert` 后自动触发、以及 `model/capabilitiesChanged` 广播。三端从 `model/list` 读能力集合做 UI 过滤。

**Tech Stack:** Rust（tokio / serde / reqwest / wiremock）、React + TypeScript（vitest）、ratatui TUI。

## Global Constraints

- 设计文档：`docs/superpowers/specs/2026-10-19-reasoning-effort-capability-probe-design.md`（下称 spec）。
- 档位白名单固定 7 档，顺序固定：`none, minimal, low, medium, high, xhigh, max`。
- 通道键固定：`"chat"` / `"responses"` / `"anthropic"`。
- 探测请求必须带一个 dummy tool（`noop`），因为「tools + effort」才是真实 agent 请求的形状。
- 探测**不得**复用 `Provider::call_stream`：它会在 effort 被拒时剥离重投、在 responses 缺失时回退 chat。
- 探测串行：单请求超时 10s，整轮总预算 30s。
- 能力未知（无记录或 `error` 非空）时，所有行为与今天完全一致（零回归）。
- 不在 `main` 分支直接改代码；本计划在 worktree `.worktrees/feat-reasoning-capability`（分支 `feat/reasoning-capability`）内执行。
- 提交前在 `yi-agent-rs/` 下跑 `cargo fmt --all`。
- Commit message 用 conventional commits，不写 `Co-Authored-By`。
- 测试按 crate 跑，不要 `cargo test --workspace`；跑前先 `ps aux | grep cargo` 确认无残留进程。

## File Structure

**新增**

| 文件 | 职责 |
|---|---|
| `yi-agent-rs/crates/yi-agent-llm/src/probe.rs` | 裸 HTTP 探测：单档判定、`auto` 通道判定、请求体构造 |
| `yi-agent-rs/crates/yi-agent-runtime/src/reasoning_probe.rs` | 探测编排：基线 + 7 档、预算、结果写回与防覆盖 |

**修改**

| 文件 | 改动 |
|---|---|
| `yi-agent-rs/crates/yi-agent-runtime/src/models.rs` | `ReasoningCapabilities` 类型、读路径容错、`upsert` 保留/清空 |
| `yi-agent-rs/crates/yi-agent-runtime/src/lib.rs` | 导出新模块 |
| `yi-agent-rs/crates/yi-agent-llm/src/lib.rs` | 导出 `probe` |
| `yi-agent-rs/crates/yi-agent-llm/src/anthropic/error.rs` | 新增 `is_effort_rejection` |
| `yi-agent-rs/crates/yi-agent-app-server/src/model_rpc.rs` | `model/list` 带能力字段、`model/probe` 分发、`upsert` 触发探测 |
| `yi-agent-rs/crates/yi-agent-app-server/src/rpc/model.rs` | `model/probe` 分支（Control 档） |
| `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs` | `model/capabilitiesChanged` 通知 |
| `yi-agent-rs/crates/yi-agent-app-server/src/rpc/thread.rs` | `thread/setReasoningEffort` 按能力校验 |
| `desktop/src/lib/models.ts` | 能力类型与 `probeModel` |
| `desktop/src/lib/modelPicker.ts` | `buildEffortOptions` 接受 `supported`；新增 `entrySupportedEfforts` |
| `desktop/src/components/ModelPicker.tsx` | 传能力集合 |
| `desktop/src/components/SettingsModelsTab.tsx` | 能力列、下拉过滤、重新探测按钮 |
| `yi-agent-rs/crates/yi-agent/src/tui/slash.rs` | `/effort` 按能力校验 |
| `docs/project-management/yi-agent-llm.md`、`yi-agent-runtime.md`、`yi-agent-app-server.md`、`desktop.md` | 进度条目 |

---

### Task 1: 能力数据模型与读路径容错

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/models.rs`
- Test: `yi-agent-rs/crates/yi-agent-runtime/src/models.rs`（同文件 `mod tests`）

**Interfaces:**
- Produces:
  - `pub struct ChannelCapability { pub supported: Vec<String>, pub probed_at: u64, pub error: Option<String> }`
  - `pub struct ReasoningCapabilities { pub chat: Option<ChannelCapability>, pub responses: Option<ChannelCapability>, pub anthropic: Option<ChannelCapability> }`
  - `ReasoningCapabilities::channel_mut(&mut self, channel: &str) -> Option<&mut Option<ChannelCapability>>`
  - `ReasoningCapabilities::get(&self, channel: &str) -> Option<&ChannelCapability>`
  - `ModelEntry.reasoning_capabilities: Option<ReasoningCapabilities>`
  - `pub const REASONING_TIERS: [&str; 7]`

- [ ] **Step 1: 写失败测试**

在 `models.rs` 的 `mod tests` 末尾加：

```rust
#[test]
fn reasoning_capabilities_roundtrip_and_drop_unknown_tiers() {
    let json = r#"{
        "models":[{
            "name":"A","provider":"openai","api_url":"https://a","model":"m",
            "reasoning_capabilities":{
                "chat":{"supported":["none","bogus","low"],"probed_at":123},
                "responses":{"supported":["high"],"probed_at":456,"error":"401"}
            }
        }]
    }"#;
    let catalog = parse_catalog(json, Path::new("x")).unwrap();
    let caps = catalog.models[0].reasoning_capabilities.as_ref().unwrap();
    assert_eq!(caps.chat.as_ref().unwrap().supported, vec!["none", "low"]);
    assert_eq!(caps.chat.as_ref().unwrap().probed_at, 123);
    assert_eq!(caps.responses.as_ref().unwrap().error.as_deref(), Some("401"));
    assert!(caps.anthropic.is_none());
}

#[test]
fn bad_capabilities_values_degrade_to_unknown_without_dropping_the_entry() {
    // 非对象 / 非数组 / 非数字都不得让整条被跳过。
    for raw in [
        r#""nope""#,
        r#"[]"#,
        r#"{"chat":"nope"}"#,
        r#"{"chat":{"supported":"nope","probed_at":"x"}}"#,
    ] {
        let json = format!(
            r#"{{"models":[{{"name":"A","provider":"openai","api_url":"https://a","model":"m","reasoning_capabilities":{raw}}}]}}"#
        );
        let catalog = parse_catalog(&json, Path::new("x")).unwrap();
        assert_eq!(catalog.models.len(), 1, "entry must survive: {raw}");
        let caps = catalog.models[0].reasoning_capabilities.as_ref();
        assert!(
            caps.map(|c| c.chat.is_none()).unwrap_or(true),
            "bad chat value must read as unknown: {raw}"
        );
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd .worktrees/feat-reasoning-capability/yi-agent-rs && cargo test -p yi-agent-runtime --lib reasoning_capabilities_roundtrip
```
Expected: 编译失败，`ReasoningCapabilities` 不存在。

- [ ] **Step 3: 实现类型与容错反序列化**

在 `models.rs` 的 `ModelEntry` 之前插入：

```rust
/// 七档白名单，顺序即 UI 顺序（与 `desktop/src/lib/modelPicker.ts` 的
/// `REASONING_EFFORTS` 逐字一致）。
pub const REASONING_TIERS: [&str; 7] =
    ["none", "minimal", "low", "medium", "high", "xhigh", "max"];

/// 某条目在某条通道上的 effort 探测结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelCapability {
    /// 探测为支持的档位，按 [`REASONING_TIERS`] 顺序、已去重。
    #[serde(default, deserialize_with = "de_supported_tiers")]
    pub supported: Vec<String>,
    /// 探测完成时间（Unix 秒）。坏值读作 0。
    #[serde(default, deserialize_with = "de_u64_lossy")]
    pub probed_at: u64,
    /// 非空 = 该通道探测未能完成（鉴权/网络/网关异常），`supported` 不可信。
    #[serde(default, deserialize_with = "de_optional_string")]
    pub error: Option<String>,
}

impl Default for ChannelCapability {
    fn default() -> Self {
        Self {
            supported: Vec::new(),
            probed_at: 0,
            error: None,
        }
    }
}

/// 三条通道各一份结果。未探测的通道为 `None`。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningCapabilities {
    #[serde(default, deserialize_with = "de_optional_capability")]
    pub chat: Option<ChannelCapability>,
    #[serde(default, deserialize_with = "de_optional_capability")]
    pub responses: Option<ChannelCapability>,
    #[serde(default, deserialize_with = "de_optional_capability")]
    pub anthropic: Option<ChannelCapability>,
}

impl ReasoningCapabilities {
    /// 通道键（`chat` / `responses` / `anthropic`）对应的槽位。
    pub fn channel_mut(&mut self, channel: &str) -> Option<&mut Option<ChannelCapability>> {
        match channel {
            "chat" => Some(&mut self.chat),
            "responses" => Some(&mut self.responses),
            "anthropic" => Some(&mut self.anthropic),
            _ => None,
        }
    }

    pub fn get(&self, channel: &str) -> Option<&ChannelCapability> {
        match channel {
            "chat" => self.chat.as_ref(),
            "responses" => self.responses.as_ref(),
            "anthropic" => self.anthropic.as_ref(),
            _ => None,
        }
    }

    /// 是否存在任一「可信」结果（有记录且 `error` 为空）。
    pub fn has_trustworthy(&self) -> bool {
        [&self.chat, &self.responses, &self.anthropic]
            .into_iter()
            .flatten()
            .any(|c| c.error.is_none())
    }
}

/// 只保留白名单内的档位，并按白名单顺序排序去重。坏值读作空表。
fn de_supported_tiers<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    let Some(items) = value.and_then(|v| v.as_array().cloned()) else {
        return Ok(Vec::new());
    };
    let mut kept: Vec<String> = items
        .iter()
        .filter_map(|v| v.as_str())
        .filter(|s| REASONING_TIERS.contains(s))
        .map(str::to_string)
        .collect();
    kept.sort_by_key(|s| REASONING_TIERS.iter().position(|t| t == s).unwrap_or(usize::MAX));
    kept.dedup();
    Ok(kept)
}

/// 非对象 / `null` → `None`；对象 → `Some(ChannelCapability)`。
fn de_optional_capability<'de, D>(deserializer: D) -> Result<Option<ChannelCapability>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    match value {
        Some(serde_json::Value::Object(_)) => serde_json::from_value(value.unwrap())
            .map(Some)
            .map_err(serde::de::Error::custom),
        _ => Ok(None),
    }
}

/// 非数字 → 0。
fn de_u64_lossy<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.and_then(|v| v.as_u64()).unwrap_or(0))
}
```

给 `ModelEntry` 加字段（放在 `context_length` 之前）：

```rust
    /// 各通道的 effort 探测结果；`None` = 从未探测。坏值按「未探测」处理。
    #[serde(default, deserialize_with = "de_optional_capabilities")]
    pub reasoning_capabilities: Option<ReasoningCapabilities>,
```

并加：

```rust
fn de_optional_capabilities<'de, D>(
    deserializer: D,
) -> Result<Option<ReasoningCapabilities>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    match value {
        Some(serde_json::Value::Object(_)) => serde_json::from_value(value.unwrap())
            .map(Some)
            .map_err(serde::de::Error::custom),
        _ => Ok(None),
    }
}
```

**注意**：`ModelEntry` 新增字段后，所有构造点会编译失败。用编译器逐个补齐：
`grep -rn "ModelEntry {" yi-agent-rs/crates --include=*.rs` 找到全部构造点，统一加
`reasoning_capabilities: None,`。

- [ ] **Step 4: 跑测试确认通过**

```bash
cd .worktrees/feat-reasoning-capability/yi-agent-rs && cargo test -p yi-agent-runtime --lib reasoning_capabilities bad_capabilities
```
Expected: 两个测试 PASS。

- [ ] **Step 5: 跑该 crate 全量测试确认无回归**

```bash
cd .worktrees/feat-reasoning-capability/yi-agent-rs && cargo test -p yi-agent-runtime
```
Expected: 全绿。

- [ ] **Step 6: 提交**

```bash
cd .worktrees/feat-reasoning-capability && cargo fmt --all --manifest-path yi-agent-rs/Cargo.toml && git add -A && git commit -m "feat(runtime): 模型条目记录各通道的 reasoning effort 能力"
```

---

### Task 2: upsert 的能力保留与清空语义

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/model_rpc.rs:110-190`
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/model_rpc.rs`（同文件 `mod tests`）

**Interfaces:**
- Consumes: Task 1 的 `ModelEntry.reasoning_capabilities`
- Produces:
  - `fn connection_fields_changed(old: &ModelEntry, new_provider: ModelProvider, api_url: &str, model: &str, api_style: Option<&str>, api_key: &str) -> bool`

- [ ] **Step 1: 写失败测试**

在 `model_rpc.rs` 的 `mod tests` 末尾加：

```rust
#[test]
fn upsert_keeps_capabilities_when_connection_fields_are_unchanged() {
    let (_dir, path) = catalog_path();
    call(&path, "model/upsert", entry_json("A")).unwrap();
    // 手工写入一份能力结果（模拟探测完成）。
    let mut catalog = load_catalog_from(&path);
    catalog.models[0].reasoning_capabilities = Some(yi_agent_runtime::models::ReasoningCapabilities {
        anthropic: Some(yi_agent_runtime::models::ChannelCapability {
            supported: vec!["low".into(), "high".into()],
            probed_at: 42,
            error: None,
        }),
        ..Default::default()
    });
    save_catalog_to(&path, &catalog).unwrap();

    // 只改 key（key 不参与能力有效性判定）。
    call(&path, "model/upsert", json!({
        "name":"A","provider":"anthropic","api_url":"https://a","model":"m","api_key":"sk-new-9999"
    })).unwrap();

    let after = load_catalog_from(&path);
    let caps = after.models[0].reasoning_capabilities.as_ref().unwrap();
    assert_eq!(caps.anthropic.as_ref().unwrap().supported, vec!["low", "high"]);
}

#[test]
fn upsert_clears_capabilities_when_connection_fields_change() {
    for change in [
        json!({"provider":"openai"}),
        json!({"api_url":"https://b"}),
        json!({"model":"m2"}),
        json!({"api_style":"chat"}),
    ] {
        let (_dir, path) = catalog_path();
        call(&path, "model/upsert", entry_json("A")).unwrap();
        let mut catalog = load_catalog_from(&path);
        catalog.models[0].reasoning_capabilities = Some(yi_agent_runtime::models::ReasoningCapabilities {
            anthropic: Some(yi_agent_runtime::models::ChannelCapability {
                supported: vec!["low".into()],
                probed_at: 42,
                error: None,
            }),
            ..Default::default()
        });
        save_catalog_to(&path, &catalog).unwrap();

        let mut payload = json!({
            "name":"A","provider":"anthropic","api_url":"https://a","model":"m"
        });
        for (k, v) in change.as_object().unwrap() {
            payload[k] = v.clone();
        }
        call(&path, "model/upsert", payload).unwrap();

        let after = load_catalog_from(&path);
        assert!(
            after.models[0].reasoning_capabilities.is_none(),
            "changing {change} must clear capabilities"
        );
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd .worktrees/feat-reasoning-capability/yi-agent-rs && cargo test -p yi-agent-app-server --lib upsert_keeps_capabilities
```
Expected: FAIL —— 现有 `upsert` 直接构造新 `ModelEntry`，能力被清空。

- [ ] **Step 3: 实现保留/清空**

在 `model_rpc.rs` 的 `upsert` 中，把 `let entry = ModelEntry { ... }` 改成：

```rust
    // 能力结果只在「连接字段未变」时保留；任一变化都说明旧探测不再可信。
    let reasoning_capabilities = match existing {
        Some(i) => {
            let old = &catalog.models[i];
            let changed = old.provider != provider
                || old.api_url != api_url
                || old.model != model
                || old.api_style != api_style;
            if changed {
                None
            } else {
                old.reasoning_capabilities.clone()
            }
        }
        None => None,
    };

    let entry = ModelEntry {
        name,
        provider,
        api_url,
        model,
        api_key,
        api_style,
        reasoning_effort,
        reasoning_capabilities,
        context_length: None,
    };
```

**注意**：`api_key` 变化**不**清空能力（换密钥不影响模型支持哪些档位）。

- [ ] **Step 4: 跑测试确认通过**

```bash
cd .worktrees/feat-reasoning-capability/yi-agent-rs && cargo test -p yi-agent-app-server --lib upsert_
```
Expected: 两个新测试 + 既有 upsert 测试全绿。

- [ ] **Step 5: 提交**

```bash
cd .worktrees/feat-reasoning-capability && cargo fmt --all --manifest-path yi-agent-rs/Cargo.toml && git add -A && git commit -m "feat(app-server): upsert 保留未失效的 effort 能力数据"
```

---

### Task 3: 裸 HTTP 探测（`yi-agent-llm`）

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-llm/src/probe.rs`
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/lib.rs`
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/anthropic/error.rs`
- Test: `yi-agent-rs/crates/yi-agent-llm/src/probe.rs`（同文件 `mod tests`，用 wiremock）

**Interfaces:**
- Produces:
  - `pub enum ProbeChannel { Chat, Responses, Anthropic }`
  - `pub enum ProbeVerdict { Supported, Unsupported, Indeterminate(String), Fatal(String) }`
  - `pub struct EffortProbeTarget<'a> { pub base_url: &'a str, pub api_key: &'a str, pub model: &'a str, pub channel: ProbeChannel }`
  - `pub async fn probe_effort(target: &EffortProbeTarget<'_>, effort: Option<&str>, timeout: Duration) -> ProbeVerdict`
  - `pub async fn detect_openai_channel(base_url: &str, api_key: &str, model: &str, timeout: Duration) -> Result<ProbeChannel, String>`
  - `pub fn probe_tools() -> Vec<yi_agent_core::ToolSchema>`

- [ ] **Step 1: 写失败测试**

新建 `probe.rs`，先只放测试与最小骨架声明（编译不过即为红）：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn target<'a>(base: &'a str, channel: ProbeChannel) -> EffortProbeTarget<'a> {
        EffortProbeTarget {
            base_url: base,
            api_key: "sk-test",
            model: "m",
            channel,
        }
    }

    #[tokio::test]
    async fn a_2xx_means_supported_and_the_request_carries_a_tool() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(body_string_contains("\"reasoning_effort\":\"high\""))
            .and(body_string_contains("\"noop\""))
            .respond_with(ResponseTemplate::new(200).set_body_string("data: [DONE]\n\n"))
            .mount(&server)
            .await;
        let v = probe_effort(&target(&server.uri(), ProbeChannel::Chat), Some("high"), Duration::from_secs(5)).await;
        assert_eq!(v, ProbeVerdict::Supported);
    }

    #[tokio::test]
    async fn an_effort_naming_400_means_unsupported() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_string(
                r#"{"error":{"message":"Unsupported value","param":"reasoning_effort"}}"#,
            ))
            .mount(&server)
            .await;
        let v = probe_effort(&target(&server.uri(), ProbeChannel::Chat), Some("minimal"), Duration::from_secs(5)).await;
        assert_eq!(v, ProbeVerdict::Unsupported);
    }

    #[tokio::test]
    async fn a_400_not_naming_effort_is_indeterminate() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_string(
                r#"{"error":{"message":"tools[0].parameters is invalid"}}"#,
            ))
            .mount(&server)
            .await;
        let v = probe_effort(&target(&server.uri(), ProbeChannel::Chat), Some("high"), Duration::from_secs(5)).await;
        assert!(matches!(v, ProbeVerdict::Indeterminate(_)));
    }

    #[tokio::test]
    async fn a_401_is_fatal() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(401).set_body_string("nope"))
            .mount(&server)
            .await;
        let v = probe_effort(&target(&server.uri(), ProbeChannel::Chat), Some("high"), Duration::from_secs(5)).await;
        assert!(matches!(v, ProbeVerdict::Fatal(_)));
    }

    #[tokio::test]
    async fn a_timeout_is_fatal() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200).set_delay(Duration::from_secs(2)),
            )
            .mount(&server)
            .await;
        let v = probe_effort(&target(&server.uri(), ProbeChannel::Chat), Some("high"), Duration::from_millis(100)).await;
        assert!(matches!(v, ProbeVerdict::Fatal(_)));
    }

    #[tokio::test]
    async fn responses_channel_posts_to_the_responses_path() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .and(body_string_contains("\"effort\":\"low\""))
            .respond_with(ResponseTemplate::new(200).set_body_string("event: done\n\n"))
            .mount(&server)
            .await;
        let v = probe_effort(&target(&server.uri(), ProbeChannel::Responses), Some("low"), Duration::from_secs(5)).await;
        assert_eq!(v, ProbeVerdict::Supported);
    }

    #[tokio::test]
    async fn anthropic_channel_posts_to_v1_messages() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(body_string_contains("\"effort\":\"medium\""))
            .respond_with(ResponseTemplate::new(200).set_body_string("event: done\n\n"))
            .mount(&server)
            .await;
        let v = probe_effort(&target(&server.uri(), ProbeChannel::Anthropic), Some("medium"), Duration::from_secs(5)).await;
        assert_eq!(v, ProbeVerdict::Supported);
    }

    #[tokio::test]
    async fn auto_detection_reads_a_missing_responses_route_as_chat_only() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(404).set_body_string(""))
            .mount(&server)
            .await;
        let ch = detect_openai_channel(&server.uri(), "sk-test", "m", Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(ch, ProbeChannel::Chat);
    }

    #[tokio::test]
    async fn auto_detection_reads_a_live_responses_route_as_responses() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_string("event: done\n\n"))
            .mount(&server)
            .await;
        let ch = detect_openai_channel(&server.uri(), "sk-test", "m", Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(ch, ProbeChannel::Responses);
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd .worktrees/feat-reasoning-capability/yi-agent-rs && cargo test -p yi-agent-llm --lib probe::tests
```
Expected: 编译失败（类型/函数未定义）。

- [ ] **Step 3: 实现探测**

在 `probe.rs` 顶部（测试模块之前）写：

```rust
//! 裸 HTTP 的 effort 探测。
//!
//! **刻意不复用 `Provider::call_stream`**：那条路径在 effort 被网关拒绝时会
//! 剥离字段重投并返回成功，在 `/responses` 缺失时会回退 chat 并返回成功。用
//! 它探测只会得到「全部支持」的假象。这里只看状态码与错误体，不读流内容。

use std::time::Duration;

use yi_agent_core::{GenParams, Message, ProviderRequest, ToolSchema};

use crate::openai::channel::looks_like_missing_route;
use crate::openai::error::is_effort_rejection as openai_effort_rejection;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeChannel {
    Chat,
    Responses,
    Anthropic,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeVerdict {
    /// 2xx：该档可用。
    Supported,
    /// 400/422 且错误体点名 effort：该档被明确拒绝。
    Unsupported,
    /// 400/422 但没点名 effort：无法归因，按未知处理。
    Indeterminate(String),
    /// 鉴权/网络/服务端错误：整轮探测应中止。
    Fatal(String),
}

pub struct EffortProbeTarget<'a> {
    pub base_url: &'a str,
    pub api_key: &'a str,
    pub model: &'a str,
    pub channel: ProbeChannel,
}

/// 探测专用 dummy tool：真实 agent 请求永远带 tools，Luna/Sol 的 400 正是
/// 「tools + effort」组合触发，不带 tool 测出来的结论在真实对话里不成立。
pub fn probe_tools() -> Vec<ToolSchema> {
    vec![ToolSchema {
        name: "noop".into(),
        description: "noop".into(),
        input_schema: serde_json::json!({"type": "object", "properties": {}}),
    }]
}

fn probe_request(model: &str, effort: Option<&str>) -> ProviderRequest {
    ProviderRequest {
        model: model.to_string(),
        system: None,
        messages: vec![Message::user("ping")],
        tools: probe_tools(),
        params: GenParams {
            max_tokens: Some(64),
            reasoning_effort: effort.map(str::to_string),
            ..Default::default()
        },
    }
}

pub async fn probe_effort(
    target: &EffortProbeTarget<'_>,
    effort: Option<&str>,
    timeout: Duration,
) -> ProbeVerdict {
    let client = match reqwest::Client::builder().timeout(timeout).build() {
        Ok(c) => c,
        Err(e) => return ProbeVerdict::Fatal(format!("failed to build HTTP client: {e}")),
    };
    let request = probe_request(target.model, effort);
    let base = target.base_url.trim_end_matches('/');

    let (url, body, auth): (String, serde_json::Value, fn(&mut reqwest::RequestBuilder, &str)) =
        match target.channel {
            ProbeChannel::Chat => (
                format!("{base}/chat/completions"),
                serde_json::to_value(crate::openai::types::OpenaiRequest::from(request))
                    .unwrap_or_default(),
                |b, key| {
                    b.header("Authorization", format!("Bearer {key}"));
                },
            ),
            ProbeChannel::Responses => (
                format!("{base}/responses"),
                serde_json::to_value(crate::openai::responses::types::responses_request_from(request))
                    .unwrap_or_default(),
                |b, key| {
                    b.header("Authorization", format!("Bearer {key}"));
                },
            ),
            ProbeChannel::Anthropic => (
                format!("{base}/v1/messages"),
                serde_json::to_value(crate::anthropic::types::AnthropicRequest::from(request))
                    .unwrap_or_default(),
                |b, key| {
                    b.header("x-api-key", key)
                        .header("anthropic-version", "2023-06-01");
                },
            ),
        };

    let mut builder = client
        .post(url)
        .header("content-type", "application/json")
        .json(&body);
    auth(&mut builder, target.api_key);
    let resp = match builder.send().await {
        Ok(r) => r,
        Err(e) => return ProbeVerdict::Fatal(format!("network error: {e}")),
    };

    let status = resp.status().as_u16();
    if (200..300).contains(&status) {
        // 不读流：拿到 2xx 就丢弃连接，省 token。
        drop(resp);
        return ProbeVerdict::Supported;
    }
    let body = resp.text().await.unwrap_or_default();
    classify(target.channel, status, &body)
}

fn classify(channel: ProbeChannel, status: u16, body: &str) -> ProbeVerdict {
    if status == 400 || status == 422 {
        let effort_named = match channel {
            ProbeChannel::Chat | ProbeChannel::Responses => openai_effort_rejection(status, body),
            ProbeChannel::Anthropic => crate::anthropic::error::is_effort_rejection(status, body),
        };
        if effort_named {
            return ProbeVerdict::Unsupported;
        }
        return ProbeVerdict::Indeterminate(truncate(body, 160));
    }
    ProbeVerdict::Fatal(format!("HTTP {status}: {}", truncate(body, 160)))
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max).collect::<String>() + "…"
}

/// `auto` 的通道判定：只发一次 `/responses`，按「404/405 且 body 为空」
/// 判定网关无该路由。判不出来（鉴权/网络）时返回可读原因。
pub async fn detect_openai_channel(
    base_url: &str,
    api_key: &str,
    model: &str,
    timeout: Duration,
) -> Result<ProbeChannel, String> {
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;
    let base = base_url.trim_end_matches('/');
    let body = serde_json::to_value(crate::openai::responses::types::responses_request_from(
        probe_request(model, None),
    ))
    .unwrap_or_default();
    let resp = client
        .post(format!("{base}/responses"))
        .header("Authorization", format!("Bearer {api_key}"))
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("network error: {e}"))?;
    let status = resp.status().as_u16();
    if (200..300).contains(&status) {
        drop(resp);
        return Ok(ProbeChannel::Responses);
    }
    let text = resp.text().await.unwrap_or_default();
    if looks_like_missing_route(status, &text) {
        return Ok(ProbeChannel::Chat);
    }
    Err(format!("HTTP {status}: {}", truncate(&text, 160)))
}
```

在 `lib.rs` 加 `pub mod probe;`，并确认 `openai::types`、`openai::responses::types`、
`anthropic::types` 三个模块与 `OpenaiRequest` / `responses_request_from` /
`AnthropicRequest` 的可见性足以从 `probe` 访问（同 crate 内 `pub(crate)` 或 `pub`
均可；若当前是私有的，就地提升为 `pub(crate)`，不扩大对外 API）。

在 `anthropic/error.rs` 加：

```rust
/// 网关是否在点名「effort / output_config」拒绝该请求。
///
/// 与 OpenAI 侧 [`crate::openai::error::is_effort_rejection`] 同形：只看
/// 400/422，且 body 必须同时点名字段与「不支持」语义，避免把普通参数错误
/// 误判成档位不支持。
pub fn is_effort_rejection(status: u16, body: &str) -> bool {
    matches!(status, 400 | 422)
        && (body.contains("\"output_config\"") || body.contains("effort"))
        && (body.contains("unsupported")
            || body.contains("does not support")
            || body.contains("not supported")
            || body.contains("invalid"))
}
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd .worktrees/feat-reasoning-capability/yi-agent-rs && cargo test -p yi-agent-llm --lib probe::tests
```
Expected: 9 个测试全 PASS。

- [ ] **Step 5: 判别性验证（证明探测绕开了兜底）**

临时把 `probe_effort` 的实现改成调用 `OpenaiProvider::call_stream`（或用
`git stash` 保存后改），跑 `an_effort_naming_400_means_unsupported`：
Expected: FAIL（`call_stream` 剥离重投后返回成功，判成 `Supported`）。
确认后恢复实现。

- [ ] **Step 6: 跑该 crate 全量测试**

```bash
cd .worktrees/feat-reasoning-capability/yi-agent-rs && cargo test -p yi-agent-llm
```
Expected: 全绿。

- [ ] **Step 7: 提交**

```bash
cd .worktrees/feat-reasoning-capability && cargo fmt --all --manifest-path yi-agent-rs/Cargo.toml && git add -A && git commit -m "feat(llm): 裸 HTTP 的 effort 能力探测，绕开剥离重投兜底"
```

---

### Task 4: 探测编排（`yi-agent-runtime`）

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-runtime/src/reasoning_probe.rs`
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/lib.rs`
- Test: `yi-agent-rs/crates/yi-agent-runtime/src/reasoning_probe.rs`（同文件 `mod tests`，wiremock）

**Interfaces:**
- Consumes: Task 3 的 `probe_effort` / `detect_openai_channel` / `ProbeChannel` / `ProbeVerdict`；Task 1 的 `ReasoningCapabilities` / `ChannelCapability` / `REASONING_TIERS`
- Produces:
  - `pub struct ProbeOutcome { pub channel: String, pub capability: ChannelCapability }`
  - `pub async fn probe_entry(provider: ModelProvider, api_url: &str, api_key: &str, model: &str, api_style: Option<&str>) -> ProbeOutcome`
  - `pub const PROBE_REQUEST_TIMEOUT: Duration`（10s）
  - `pub const PROBE_TOTAL_BUDGET: Duration`（30s）

- [ ] **Step 1: 写失败测试**

新建 `reasoning_probe.rs`，先只放测试：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// 基线 + 指定档位的响应：命中 `reasoning_effort` 时按 `verdict` 回。
    async fn server_with(tiers: &[(&str, u16)]) -> MockServer {
        let server = MockServer::start().await;
        for (tier, status) in tiers {
            Mock::given(method("POST"))
                .and(path("/chat/completions"))
                .and(body_string_contains(&format!("\"reasoning_effort\":\"{tier}\"")))
                .respond_with(ResponseTemplate::new(*status).set_body_string(
                    if *status == 200 {
                        "data: [DONE]\n\n".to_string()
                    } else {
                        r#"{"error":{"message":"Unsupported value","param":"reasoning_effort"}}"#
                            .to_string()
                    },
                ))
                .mount(&server)
                .await;
        }
        // 基线（不带 reasoning_effort）与未命中档位的兜底。
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string("data: [DONE]\n\n"))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn only_the_accepted_tiers_are_recorded() {
        let server = server_with(&[("low", 200), ("medium", 400), ("high", 200)]).await;
        let out = probe_entry(
            ModelProvider::Openai,
            &server.uri(),
            "sk-test",
            "m",
            Some("chat"),
        )
        .await;
        assert_eq!(out.channel, "chat");
        assert_eq!(out.capability.supported, vec!["low", "high"]);
        assert!(out.capability.error.is_none());
        assert!(out.capability.probed_at > 0);
    }

    #[tokio::test]
    async fn an_anthropic_entry_probes_the_anthropic_channel_only() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_string("event: done\n\n"))
            .mount(&server)
            .await;
        let out = probe_entry(
            ModelProvider::Anthropic,
            &server.uri(),
            "sk-test",
            "claude",
            None,
        )
        .await;
        assert_eq!(out.channel, "anthropic");
        assert_eq!(out.capability.supported.len(), 7);
    }

    #[tokio::test]
    async fn a_failing_baseline_stops_immediately_and_records_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(401).set_body_string("nope"))
            .mount(&server)
            .await;
        let out = probe_entry(
            ModelProvider::Openai,
            &server.uri(),
            "sk-test",
            "m",
            Some("chat"),
        )
        .await;
        assert!(out.capability.supported.is_empty());
        assert!(out.capability.error.is_some());
        // 只发了基线那一次。
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn auto_style_probes_only_the_detected_channel() {
        let server = MockServer::start().await;
        // /responses 缺失 → 判定为 chat-only。
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(404).set_body_string(""))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string("data: [DONE]\n\n"))
            .mount(&server)
            .await;
        let out = probe_entry(ModelProvider::Openai, &server.uri(), "sk-test", "m", None).await;
        assert_eq!(out.channel, "chat");
        assert_eq!(out.capability.supported.len(), 7);
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd .worktrees/feat-reasoning-capability/yi-agent-rs && cargo test -p yi-agent-runtime --lib reasoning_probe::tests
```
Expected: 编译失败。

- [ ] **Step 3: 实现编排**

在 `reasoning_probe.rs` 顶部（测试模块之前）写：

```rust
//! effort 能力探测的编排：基线 + 7 档串行、预算控制、结果封装。
//!
//! 只负责「问一轮、收一份结果」；落盘与防覆盖由调用方（app-server）按条目
//! 指纹决定。

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use yi_agent_llm::probe::{self, EffortProbeTarget, ProbeChannel, ProbeVerdict};

use crate::models::{ChannelCapability, ModelProvider, REASONING_TIERS};

/// 单个探测请求的超时。
pub const PROBE_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// 整轮探测（基线 + 7 档）的总预算。
pub const PROBE_TOTAL_BUDGET: Duration = Duration::from_secs(30);

pub struct ProbeOutcome {
    /// `"chat"` / `"responses"` / `"anthropic"`。
    pub channel: String,
    pub capability: ChannelCapability,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn channel_key(channel: ProbeChannel) -> &'static str {
    match channel {
        ProbeChannel::Chat => "chat",
        ProbeChannel::Responses => "responses",
        ProbeChannel::Anthropic => "anthropic",
    }
}

/// 对一条模型条目跑一轮探测。
///
/// `api_style` 只对 `openai` 有意义：`chat` / `responses` 直接定通道，`None`
/// （= auto）先判定网关通道再只测那一条。
pub async fn probe_entry(
    provider: ModelProvider,
    api_url: &str,
    api_key: &str,
    model: &str,
    api_style: Option<&str>,
) -> ProbeOutcome {
    let channel = match provider {
        ModelProvider::Anthropic => ProbeChannel::Anthropic,
        ModelProvider::Openai => match api_style {
            Some("chat") => ProbeChannel::Chat,
            Some("responses") => ProbeChannel::Responses,
            _ => match probe::detect_openai_channel(
                api_url,
                api_key,
                model,
                PROBE_REQUEST_TIMEOUT,
            )
            .await
            {
                Ok(ch) => ch,
                Err(error) => {
                    return ProbeOutcome {
                        channel: "chat".to_string(),
                        capability: ChannelCapability {
                            supported: Vec::new(),
                            probed_at: now_secs(),
                            error: Some(error),
                        },
                    };
                }
            },
        },
    };

    let started = Instant::now();
    let target = EffortProbeTarget {
        base_url: api_url,
        api_key,
        model,
        channel,
    };

    // 基线：不带 effort。它把「鉴权/模型不存在/网络」与「档位不支持」分开，
    // 否则 401 会被记成「7 档都不支持」。
    match probe::probe_effort(&target, None, PROBE_REQUEST_TIMEOUT).await {
        ProbeVerdict::Supported => {}
        ProbeVerdict::Unsupported | ProbeVerdict::Indeterminate(_) => {
            return ProbeOutcome {
                channel: channel_key(channel).to_string(),
                capability: ChannelCapability {
                    supported: Vec::new(),
                    probed_at: now_secs(),
                    error: Some("baseline request was rejected".to_string()),
                },
            };
        }
        ProbeVerdict::Fatal(error) => {
            return ProbeOutcome {
                channel: channel_key(channel).to_string(),
                capability: ChannelCapability {
                    supported: Vec::new(),
                    probed_at: now_secs(),
                    error: Some(error),
                },
            };
        }
    }

    let mut supported: Vec<String> = Vec::new();
    let mut fatal: Option<String> = None;
    for tier in REASONING_TIERS {
        if started.elapsed() >= PROBE_TOTAL_BUDGET {
            // 预算耗尽：保留已得结果，剩余档位按未知处理（不算不支持）。
            break;
        }
        match probe::probe_effort(&target, Some(tier), PROBE_REQUEST_TIMEOUT).await {
            ProbeVerdict::Supported => supported.push(tier.to_string()),
            ProbeVerdict::Unsupported => {}
            // 无法归因：不写进 supported，也不中止其余档位。
            ProbeVerdict::Indeterminate(_) => {}
            ProbeVerdict::Fatal(error) => {
                fatal = Some(error);
                break;
            }
        }
    }

    ProbeOutcome {
        channel: channel_key(channel).to_string(),
        capability: ChannelCapability {
            supported,
            probed_at: now_secs(),
            error: fatal,
        },
    }
}
```

在 `lib.rs` 加 `pub mod reasoning_probe;`。

- [ ] **Step 4: 跑测试确认通过**

```bash
cd .worktrees/feat-reasoning-capability/yi-agent-rs && cargo test -p yi-agent-runtime --lib reasoning_probe::tests
```
Expected: 4 个测试全 PASS。

- [ ] **Step 5: 跑该 crate 全量测试**

```bash
cd .worktrees/feat-reasoning-capability/yi-agent-rs && cargo test -p yi-agent-runtime
```
Expected: 全绿。

- [ ] **Step 6: 提交**

```bash
cd .worktrees/feat-reasoning-capability && cargo fmt --all --manifest-path yi-agent-rs/Cargo.toml && git add -A && git commit -m "feat(runtime): effort 探测编排（基线 + 7 档、预算控制）"
```

---

### Task 5: `model/probe` RPC、广播与自动触发

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/model_rpc.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/model.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（探测任务登记表）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/model_rpc.rs`、`protocol.rs`、`rpc_tests.rs`

**Interfaces:**
- Consumes: Task 4 的 `probe_entry`；Task 1 的 `ReasoningCapabilities`
- Produces:
  - `pub fn probe_started(state: &ProbeRegistry, name: &str) -> bool`
  - `pub fn probe_finished(state: &ProbeRegistry, name: &str)`
  - `pub struct ProbeRegistry(Arc<Mutex<HashSet<String>>>)`
  - `pub fn apply_probe_result(path: &Path, name: &str, fingerprint: &str, outcome: ProbeOutcome) -> bool`
  - `Notification::ModelCapabilitiesChanged { name: String, reasoning_capabilities: Option<ReasoningCapabilities> }`

- [ ] **Step 1: 写失败测试（协议形状）**

在 `protocol.rs` 的 `mod tests` 加：

```rust
#[test]
fn model_capabilities_changed_notification_carries_method_and_payload() {
    let n = Notification::ModelCapabilitiesChanged {
        name: "A".into(),
        reasoning_capabilities: Some(yi_agent_runtime::models::ReasoningCapabilities {
            chat: Some(yi_agent_runtime::models::ChannelCapability {
                supported: vec!["low".into()],
                probed_at: 7,
                error: None,
            }),
            ..Default::default()
        }),
    };
    let v = serde_json::to_value(crate::protocol::NotificationEnvelope::new(&n)).unwrap();
    assert_eq!(v["method"], "model/capabilitiesChanged");
    assert_eq!(v["params"]["name"], "A");
    assert_eq!(v["params"]["reasoning_capabilities"]["chat"]["supported"][0], "low");
    // 机器级清单属性：无 thread 键，恒推。
    assert!(n.thread_key().is_none());
}
```

- [ ] **Step 2: 写失败测试（写回防覆盖）**

在 `model_rpc.rs` 的 `mod tests` 加：

```rust
#[test]
fn apply_probe_result_writes_only_when_the_fingerprint_matches() {
    let (_dir, path) = catalog_path();
    call(&path, "model/upsert", entry_json("A")).unwrap();
    let fingerprint = crate::model_rpc::entry_fingerprint(
        &load_catalog_from(&path).models[0],
    );

    // 指纹不匹配 → 不写。
    let stale = ProbeOutcome {
        channel: "anthropic".into(),
        capability: yi_agent_runtime::models::ChannelCapability {
            supported: vec!["low".into()],
            probed_at: 1,
            error: None,
        },
    };
    assert!(!crate::model_rpc::apply_probe_result(
        &path, "A", "stale", stale
    ));
    assert!(load_catalog_from(&path).models[0]
        .reasoning_capabilities
        .is_none());

    // 指纹匹配 → 写入。
    let fresh = ProbeOutcome {
        channel: "anthropic".into(),
        capability: yi_agent_runtime::models::ChannelCapability {
            supported: vec!["low".into(), "high".into()],
            probed_at: 2,
            error: None,
        },
    };
    assert!(crate::model_rpc::apply_probe_result(
        &path, "A", &fingerprint, fresh
    ));
    let caps = load_catalog_from(&path).models[0]
        .reasoning_capabilities
        .clone()
        .unwrap();
    assert_eq!(caps.anthropic.unwrap().supported, vec!["low", "high"]);
}

#[test]
fn apply_probe_result_on_a_deleted_entry_is_a_no_op() {
    let (_dir, path) = catalog_path();
    call(&path, "model/upsert", entry_json("A")).unwrap();
    let fingerprint = crate::model_rpc::entry_fingerprint(
        &load_catalog_from(&path).models[0],
    );
    call(&path, "model/delete", json!({"name":"A"})).unwrap();
    let outcome = ProbeOutcome {
        channel: "anthropic".into(),
        capability: yi_agent_runtime::models::ChannelCapability::default(),
    };
    assert!(!crate::model_rpc::apply_probe_result(
        &path, "A", &fingerprint, outcome
    ));
}
```

- [ ] **Step 3: 跑测试确认失败**

```bash
cd .worktrees/feat-reasoning-capability/yi-agent-rs && cargo test -p yi-agent-app-server --lib model_capabilities_changed apply_probe_result
```
Expected: 编译失败。

- [ ] **Step 4: 实现协议、指纹与写回**

`protocol.rs`：在 `Notification` 枚举里（`Notice` 之后）加：

```rust
    /// 某模型条目的 effort 能力探测完成。
    ///
    /// 机器级清单属性（无 thread 键），故恒推给所有客户端。直接携带权威结果，
    /// 客户端就地更新内存清单，不必重拉全量 `model/list`。
    #[serde(rename = "model/capabilitiesChanged")]
    ModelCapabilitiesChanged {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_capabilities: Option<crate::model_rpc::ReasoningCapabilities>,
    },
```

在 `thread_key()` 的 `None` 分支加入该变体；在 `delivery()` 的 `Delivery::Global`
分支加入该变体。

**注意**：`Notification` 带 `#[ts(export)]`，新增变体后必须重新生成 TypeScript 绑定
并**一并提交**（仓库已跟踪 `desktop/src/lib/generated/`）：

```bash
cd .worktrees/feat-reasoning-capability/yi-agent-rs && cargo test --all-features -p yi-agent-app-server export_bindings
cd .. && git add desktop/src/lib/generated
```

`just check-ts` 会校验生成物与提交物一致，不一致即红。

`model_rpc.rs` 顶部加：

```rust
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

pub use yi_agent_runtime::models::{ChannelCapability, ReasoningCapabilities};
use yi_agent_runtime::models::ModelProvider;
use yi_agent_runtime::reasoning_probe::{ProbeOutcome, probe_entry};
```

加指纹与写回：

```rust
/// 条目「连接身份」的指纹：探测期间它变了，结果就不可信。
///
/// **不含 `api_key`**：换密钥不影响模型支持哪些档位。
pub fn entry_fingerprint(entry: &ModelEntry) -> String {
    format!(
        "{}|{}|{}|{}",
        entry.provider.as_str(),
        entry.api_url,
        entry.model,
        entry.api_style.as_deref().unwrap_or("auto")
    )
}

/// 把一次探测结果写回清单。返回是否真的写入。
///
/// 写前重读并核对指纹：探测可能跑了 30 秒，期间条目可能被改或删。
pub fn apply_probe_result(
    path: &Path,
    name: &str,
    fingerprint: &str,
    outcome: ProbeOutcome,
) -> bool {
    let mut catalog = load_catalog_from(path);
    let Some(entry) = catalog.models.iter_mut().find(|m| m.name == name) else {
        return false;
    };
    if entry_fingerprint(entry) != fingerprint {
        return false;
    }
    let caps = entry.reasoning_capabilities.get_or_insert_with(Default::default);
    if let Some(slot) = caps.channel_mut(&outcome.channel) {
        *slot = Some(outcome.capability);
    } else {
        return false;
    }
    save_catalog_to(path, &catalog).is_ok()
}

/// 正在探测的条目名集合（进程内）。重复请求直接丢弃。
#[derive(Clone, Default)]
pub struct ProbeRegistry(Arc<Mutex<HashSet<String>>>);

impl ProbeRegistry {
    pub fn try_begin(&self, name: &str) -> bool {
        self.0
            .lock()
            .map(|mut set| set.insert(name.to_string()))
            .unwrap_or(false)
    }

    pub fn finish(&self, name: &str) {
        if let Ok(mut set) = self.0.lock() {
            set.remove(name);
        }
    }
}
```

- [ ] **Step 5: 实现 `model/probe` 分发**

`model_rpc.rs` 的 `handle_model_request_at` 中，`model/list` 分支前加：

```rust
        "model/probe" => {
            // 只解析条目、不写盘；真正的探测在 app-server 的 async 分支里跑。
            let name = string_field(params, "name").unwrap_or_default();
            let catalog = load_catalog_from(path);
            match catalog.models.iter().find(|m| m.name == name) {
                Some(_) => Ok(json!({ "ok": true })),
                None => Err(RpcError::model_not_found(&name)),
            }
        }
```

`model/list` 的 `entry_view` 增加字段：

```rust
        "reasoning_capabilities": entry.reasoning_capabilities,
```

`effective_view` 的两个分支同样加 `"reasoning_capabilities": ...`（条目分支取
`entry.reasoning_capabilities`；env 分支取 `Value::Null`）。

`rpc/model.rs`：把 `"model/probe"` 加进写操作那组的 `match` 分支（`Control` 档），
但因为它要 `async`，单独开一个分支，照 `model/test` 的形状：

```rust
        "model/probe" => {
            if state.client_scope < Scope::Control {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::insufficient_scope(Scope::Control)),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            let name = req
                .params
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let catalog = yi_agent_runtime::models::load_catalog_from(state.models_path);
            let Some(entry) = catalog.models.iter().find(|m| m.name == name) else {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::model_not_found(&name)),
                )
                .await?;
                return Ok(Flow::Continue);
            };
            if !state.probes.try_begin(&name) {
                write_response(
                    state.hub,
                    state.client,
                    ok_response(id, serde_json::json!({ "started": false })),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            let fingerprint = crate::model_rpc::entry_fingerprint(entry);
            let target = (
                entry.provider,
                entry.api_url.clone(),
                entry.api_key.clone(),
                entry.model.clone(),
                entry.api_style.clone(),
            );
            let hub = state.hub.clone();
            let probes = state.probes.clone();
            let path = state.models_path.to_path_buf();
            tokio::spawn(async move {
                let (provider, api_url, api_key, model, api_style) = target;
                let outcome = yi_agent_runtime::reasoning_probe::probe_entry(
                    provider,
                    &api_url,
                    &api_key,
                    &model,
                    api_style.as_deref(),
                )
                .await;
                let applied = crate::model_rpc::apply_probe_result(
                    &path,
                    &name,
                    &fingerprint,
                    outcome,
                );
                probes.finish(&name);
                if !applied {
                    return;
                }
                let caps = yi_agent_runtime::models::load_catalog_from(&path)
                    .models
                    .into_iter()
                    .find(|m| m.name == name)
                    .and_then(|m| m.reasoning_capabilities);
                let _ = crate::server::write_notification(
                    &hub,
                    &crate::protocol::Notification::ModelCapabilitiesChanged {
                        name,
                        reasoning_capabilities: caps,
                    },
                )
                .await;
            });
            write_response(
                state.hub,
                state.client,
                ok_response(id, serde_json::json!({ "started": true })),
            )
            .await?;
        }
```

`AppState` 增加 `pub(crate) probes: &'a crate::model_rpc::ProbeRegistry`，并在
`serve()` 构造 `AppState` 的地方传入一个进程级 `ProbeRegistry::default()`。

- [ ] **Step 6: `model/upsert` 自动触发**

`rpc/model.rs` 的 `model/upsert` 分支在成功回包**之后**（不阻塞响应）比较指纹：

```rust
            // 成功回包后再决定是否触发探测：连接字段变了，旧结果已被 upsert 清空。
            let before = crate::model_rpc::load_catalog_from(state.models_path);
            // ... 调用 upsert ...
            let after = crate::model_rpc::load_catalog_from(state.models_path);
            let name = req.params.get("name").and_then(|v| v.as_str()).unwrap_or_default();
            let changed = match (before.models.iter().find(|m| m.name == name), after.models.iter().find(|m| m.name == name)) {
                (Some(old), Some(new)) => {
                    crate::model_rpc::entry_fingerprint(old) != crate::model_rpc::entry_fingerprint(new)
                        || new.reasoning_capabilities.is_none()
                }
                (None, Some(_)) => true,
                _ => false,
            };
            if changed {
                // 复用 `model/probe` 的同一条后台路径（构造一个等价请求交给同一 helper）。
            }
```

把「后台探测」抽成 `pub(crate) fn spawn_probe(hub, probes, path, name, entry)`，
`model/probe` 与 `model/upsert` 都调它，避免两份 tokio::spawn。

- [ ] **Step 7: 跑测试确认通过**

```bash
cd .worktrees/feat-reasoning-capability/yi-agent-rs && cargo test -p yi-agent-app-server --lib model_capabilities_changed apply_probe_result model_
```
Expected: 全绿。

- [ ] **Step 8: 加集成用例（探测去重 + 广播）**

在 `rpc_tests.rs` 加（照现有 `model/*` 用例的 harness）：

```rust
#[tokio::test]
async fn model_probe_is_deduplicated_while_one_is_running() {
    // 用一个会延迟 300ms 的 wiremock 作为上游，连续发两次 model/probe，
    // 断言第二次回 started:false，且只发生一次探测。
}

#[tokio::test]
async fn model_probe_broadcasts_capabilities_changed() {
    // 探测完成后，订阅端应收到 method == "model/capabilitiesChanged"，
    // 且 params.reasoning_capabilities 非空。
}
```

具体 harness 调用照抄同文件里相邻的 `model/upsert` 用例（`serve_scoped_with_loopback`
或 `serve_stdio` 的现有写法），只替换方法与断言。

- [ ] **Step 9: 提交**

```bash
cd .worktrees/feat-reasoning-capability && cargo fmt --all --manifest-path yi-agent-rs/Cargo.toml && git add -A && git commit -m "feat(app-server): model/probe RPC、能力广播与 upsert 自动触发"
```

---

### Task 6: 运行期与写入期约束

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/models.rs`（`resolve_effective`）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/thread.rs:914-940`
- Test: 两处同文件测试

**Interfaces:**
- Consumes: Task 1 的 `ReasoningCapabilities`
- Produces:
  - `pub fn effective_channel(entry: &ModelEntry) -> &'static str`
  - `pub fn supported_efforts(entry: &ModelEntry) -> Option<Vec<String>>`
  - `pub fn effort_is_allowed(entry: &ModelEntry, effort: &str) -> bool`

- [ ] **Step 1: 写失败测试**

在 `models.rs` 的 `mod tests` 加：

```rust
#[test]
fn an_unsupported_default_effort_is_dropped_at_resolve_time() {
    let mut catalog = ModelCatalog::default();
    catalog.models.push(ModelEntry {
        name: "A".into(),
        provider: ModelProvider::Openai,
        api_url: "https://a".into(),
        model: "m".into(),
        api_key: "k".into(),
        api_style: Some("chat".into()),
        reasoning_effort: Some("high".into()),
        reasoning_capabilities: Some(ReasoningCapabilities {
            chat: Some(ChannelCapability {
                supported: vec!["low".into()],
                probed_at: 1,
                error: None,
            }),
            ..Default::default()
        }),
        context_length: None,
    });
    catalog.default_model = Some("A".into());
    let cfg = test_config();
    let out = resolve_effective(&cfg, &catalog, None);
    assert_eq!(out.reasoning_effort, None, "unsupported tier must not be sent");
}

#[test]
fn an_unknown_capability_keeps_the_default_effort() {
    let entry = ModelEntry {
        name: "A".into(),
        provider: ModelProvider::Openai,
        api_url: "https://a".into(),
        model: "m".into(),
        api_key: "k".into(),
        api_style: Some("chat".into()),
        reasoning_effort: Some("high".into()),
        reasoning_capabilities: None,
        context_length: None,
    };
    assert!(effort_is_allowed(&entry, "high"), "unknown must not block");
}
```

**前置**：`RuntimeConfig` **没有** `Default` 实现，字段多达 20+。测试配置请复用
`yi-agent-app-server/src/model_rpc.rs:369` 的 `fallback_cfg()` 形状，在
`models.rs` 的测试模块里加一个同样的私有辅助函数（逐字段显式构造），命名为
`test_config()`。不要给 `RuntimeConfig` 新增 `Default`（会掩盖漏配字段）。

- [ ] **Step 2: 跑测试确认失败**

```bash
cd .worktrees/feat-reasoning-capability/yi-agent-rs && cargo test -p yi-agent-runtime --lib unsupported_default_effort
```
Expected: FAIL。

- [ ] **Step 3: 实现能力查询与运行期过滤**

`models.rs` 加：

```rust
/// 条目实际会走的通道键。
///
/// `auto` 时取「已探测且可信」的那一条；两条都有则优先 `responses`（与
/// `OpenaiProvider::resolve_channel` 的默认一致）。
pub fn effective_channel(entry: &ModelEntry) -> &'static str {
    match entry.provider {
        ModelProvider::Anthropic => "anthropic",
        ModelProvider::Openai => match entry.api_style.as_deref() {
            Some("chat") => "chat",
            Some("responses") => "responses",
            _ => {
                let caps = entry.reasoning_capabilities.as_ref();
                let trustworthy = |c: &Option<ChannelCapability>| {
                    c.as_ref().is_some_and(|c| c.error.is_none())
                };
                match caps {
                    Some(c) if trustworthy(&c.chat) && !trustworthy(&c.responses) => "chat",
                    _ => "responses",
                }
            }
        },
    }
}

/// 该条目在其生效通道上的支持档位；`None` = 能力未知（零回归）。
pub fn supported_efforts(entry: &ModelEntry) -> Option<Vec<String>> {
    let caps = entry.reasoning_capabilities.as_ref()?;
    let cap = caps.get(effective_channel(entry))?;
    if cap.error.is_some() {
        return None;
    }
    Some(cap.supported.clone())
}

/// 某档位是否可发。能力未知时恒为 `true`（保持今天的行为）。
pub fn effort_is_allowed(entry: &ModelEntry, effort: &str) -> bool {
    supported_efforts(entry)
        .map(|list| list.iter().any(|s| s == effort))
        .unwrap_or(true)
}
```

`resolve_effective` 中，在 `out.reasoning_effort = entry.reasoning_effort.clone();`
之后加：

```rust
            // 能力可信且该档不在支持列表里 → 按「未发送」处理，避免必然的 400。
            // 会话覆盖同样经过这里（调用方把覆盖写进 `cfg.reasoning_effort` 前
            // 也走同一条链）。
            if let Some(effort) = out.reasoning_effort.clone() {
                if !effort_is_allowed(entry, &effort) {
                    tracing::info!(
                        model = %entry.name,
                        %effort,
                        "configured reasoning effort is not supported; dropping it"
                    );
                    out.reasoning_effort = None;
                }
            }
```

- [ ] **Step 4: 服务端写入校验**

`rpc/thread.rs` 的 `thread/setReasoningEffort` 白名单校验之后加：

```rust
            // 条目有能力数据时，只接受它支持的档位。能力未知 → 保持七档白名单。
            if let Some(effort) = effort.as_deref() {
                let catalog = yi_agent_runtime::models::load_catalog_from(state.models_path);
                if let Some(entry) = yi_agent_runtime::models::effective_entry_for(
                    state.cfg,
                    &catalog,
                    Some(&model_ref),
                ) {
                    if let Some(allowed) = yi_agent_runtime::models::supported_efforts(entry) {
                        if !allowed.iter().any(|s| s == effort) {
                            write_response(
                                state.hub,
                                state.client,
                                err_response(
                                    id,
                                    RpcError::invalid_params(format!(
                                        "reasoning effort '{effort}' is not supported by this model; supported: {}",
                                        if allowed.is_empty() {
                                            "(none)".to_string()
                                        } else {
                                            allowed.join(", ")
                                        }
                                    )),
                                ),
                            )
                            .await?;
                            return Ok(Flow::Continue);
                        }
                    }
                }
            }
```

（`model_ref` 从该 thread 的 meta 取；若拿不到就跳过校验，保持宽松。）

- [ ] **Step 5: 跑测试确认通过**

```bash
cd .worktrees/feat-reasoning-capability/yi-agent-rs && cargo test -p yi-agent-runtime --lib effort && cargo test -p yi-agent-app-server --lib set_reasoning_effort
```
Expected: 全绿。

- [ ] **Step 6: 提交**

```bash
cd .worktrees/feat-reasoning-capability && cargo fmt --all --manifest-path yi-agent-rs/Cargo.toml && git add -A && git commit -m "feat: 运行期与写入期按能力集合约束 effort"
```

---

### Task 7: 桌面端 UI

**Files:**
- Modify: `desktop/src/lib/models.ts`
- Modify: `desktop/src/lib/modelPicker.ts`
- Modify: `desktop/src/components/ModelPicker.tsx`
- Modify: `desktop/src/components/SettingsModelsTab.tsx`
- Test: `desktop/src/lib/modelPicker.test.ts`、`desktop/src/components/SettingsModelsTab.test.tsx`、`desktop/src/components/ModelPicker.test.tsx`

**Interfaces:**
- Consumes: Task 5 的 `model/list` 字段形状
- Produces:
  - `type ChannelCapability = { supported: string[]; probed_at: number; error?: string | null }`
  - `type ReasoningCapabilities = { chat?: ChannelCapability | null; responses?: ChannelCapability | null; anthropic?: ChannelCapability | null }`
  - `ModelEntryView.reasoning_capabilities?: ReasoningCapabilities | null`
  - `buildEffortOptions(current: string | null, entryDefault: string | null, supported: string[] | null): EffortOption[]`
  - `entrySupportedEfforts(list: ModelList, ref: string | null): string[] | null`
  - `probeModel(rpc: ModelRpc, name: string): Promise<{ started: boolean }>`

- [ ] **Step 1: 写失败测试**

`desktop/src/lib/modelPicker.test.ts` 加：

```ts
it("lists every tier when capabilities are unknown", () => {
  const options = buildEffortOptions(null, null, null);
  expect(options.map((o) => o.value)).toEqual([null, ...REASONING_EFFORTS]);
});

it("lists only the supported tiers", () => {
  const options = buildEffortOptions(null, "low", ["low", "high"]);
  expect(options.map((o) => o.value)).toEqual([null, "low", "high"]);
  expect(options[0].label).toBe("未设置（继承 low）");
});

it("lists only the unset entry when the model supports no explicit tier", () => {
  const options = buildEffortOptions(null, null, []);
  expect(options.map((o) => o.value)).toEqual([null]);
});

it("keeps an unsupported selected tier visible instead of silently changing it", () => {
  const options = buildEffortOptions("max", "low", ["low"]);
  const selected = options.find((o) => o.selected);
  expect(selected?.value).toBe("max");
  expect(selected?.label).toContain("不支持");
});

it("reads the supported tiers for the entry the session resolves to", () => {
  const list: ModelList = {
    models: [
      {
        name: "A",
        provider: "openai",
        api_url: "https://a",
        model: "m",
        has_key: true,
        api_key_masked: "••••",
        api_style: "chat",
        reasoning_effort: "low",
        reasoning_capabilities: { chat: { supported: ["low"], probed_at: 1 } },
      },
    ],
    default_model: "A",
    subagent_model: null,
    effective: null,
  };
  expect(entrySupportedEfforts(list, null)).toEqual(["low"]);
});
```

`SettingsModelsTab.test.tsx` 加：

```tsx
it("renders the probed tiers and offers a re-probe", async () => {
  const call = vi.fn(async (method: string) => {
    if (method === RPC.modelList) {
      return {
        models: [
          {
            name: "A", provider: "openai", api_url: "https://a", model: "m",
            has_key: true, api_key_masked: "••••",
            reasoning_capabilities: { chat: { supported: ["low", "high"], probed_at: 1 } },
          },
        ],
        default_model: "A", subagent_model: null, effective: null,
      };
    }
    if (method === RPC.modelProbe) return { started: true };
    return {};
  });
  render(<SettingsModelsTab call={call} />);
  expect(await screen.findByText(/low/)).toBeInTheDocument();
  await userEvent.click(screen.getByRole("button", { name: "重新探测 A" }));
  expect(call).toHaveBeenCalledWith(RPC.modelProbe, { name: "A" });
});
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd .worktrees/feat-reasoning-capability/desktop && npx vitest run src/lib/modelPicker.test.ts src/components/SettingsModelsTab.test.tsx
```
Expected: FAIL。

- [ ] **Step 3: 实现类型与纯函数**

`models.ts` 加：

```ts
export type ChannelCapability = {
  supported: string[];
  probed_at: number;
  error?: string | null;
};

export type ReasoningCapabilities = {
  chat?: ChannelCapability | null;
  responses?: ChannelCapability | null;
  anthropic?: ChannelCapability | null;
};

/** 机器级探测：只启动后台任务，结果走 `model/capabilitiesChanged`。 */
export async function probeModel(rpc: ModelRpc, name: string): Promise<{ started: boolean }> {
  const result = await rpc<{ started?: unknown }>(RPC.modelProbe, { name });
  return { started: result?.started === true };
}
```

给 `ModelEntryView` 加：

```ts
  reasoning_capabilities?: ReasoningCapabilities | null;
```

`routes.ts` 加 `modelProbe: "model/probe"`。

`modelPicker.ts`：把 `buildEffortOptions` 改成：

```ts
export function buildEffortOptions(
  current: string | null,
  entryDefault: string | null,
  supported: string[] | null,
): EffortOption[] {
  const unset: EffortOption = {
    value: null,
    label: entryDefault !== null ? `未设置（继承 ${entryDefault}）` : "未设置",
    selected: current === null,
  };
  const tiers = supported ?? REASONING_EFFORTS;
  const options: EffortOption[] = tiers.map((tier) => ({
    value: tier,
    label: tier,
    selected: tier === current,
  }));
  // 当前覆盖不在支持列表里时**不隐藏**：静默改用户的配置不可预期。
  if (current !== null && !tiers.includes(current)) {
    options.push({ value: current, label: `${current}（该模型不支持）`, selected: true });
  }
  return [unset, ...options];
}

/** 条目在「生效通道」上的支持档位；`null` = 未知（UI 平铺 7 档）。 */
export function entrySupportedEfforts(
  list: ModelList,
  ref: string | null,
): string[] | null {
  const name = ref ?? list.default_model;
  if (name === null) return null;
  const entry = list.models.find((m) => m.name === name);
  const caps = entry?.reasoning_capabilities;
  if (!caps) return null;
  const channel =
    entry?.provider === "anthropic"
      ? "anthropic"
      : entry?.api_style === "chat"
        ? "chat"
        : entry?.api_style === "responses"
          ? "responses"
          : caps.chat && !caps.chat.error && !(caps.responses && !caps.responses.error)
            ? "chat"
            : "responses";
  const cap = caps[channel];
  if (!cap || cap.error) return null;
  return cap.supported;
}
```

- [ ] **Step 4: 接线到两个组件**

`ModelPicker.tsx`：

```tsx
const supported = entrySupportedEfforts(catalog, currentRef);
const effortOptions = buildEffortOptions(effortOverride, entryDefault, supported);
```

`SettingsModelsTab.tsx`：
- 编辑表单的下拉改为：

```tsx
const supported = draftSupportedEfforts(draft, catalog);
...
{supported === null
  ? REASONING_EFFORTS.map(...)
  : supported.map(...)}
{draft.reasoning_effort !== UNSET && supported !== null && !supported.includes(draft.reasoning_effort) && (
  <option value={draft.reasoning_effort}>{draft.reasoning_effort}（该模型不支持）</option>
)}
```

其中 `draftSupportedEfforts` 按 `draft.provider + draft.api_style` 从
`catalog.models` 里同名字段取（新增条目没有能力数据 → `null`）。

- 清单行/卡片的 `catalogFields` 加一项：

```tsx
{
  label: "推理档位",
  value: (entry) => {
    const caps = entry.reasoning_capabilities;
    if (!caps) return "未探测";
    const parts = (["chat", "responses", "anthropic"] as const)
      .map((ch) => {
        const cap = caps[ch];
        if (!cap) return null;
        if (cap.error) return `${ch} 未探测`;
        return `${ch} ${cap.supported.length === 0 ? "(无)" : cap.supported.join("·")}`;
      })
      .filter(Boolean);
    return parts.length === 0 ? "未探测" : parts.join("；");
  },
},
```

- 操作列/卡片按钮组加：

```tsx
<button
  type="button"
  aria-label={`重新探测 ${entry.name}`}
  onClick={() => void reprobe(entry.name)}
  className="ml-1 rounded px-2 py-0.5 text-xs text-fg-muted hover:text-fg"
>
  重新探测
</button>
```

`reprobe` 调 `probeModel(rpc, name)`，成功则按钮短暂显示「探测中…」（本地
`Set<string>` 状态），收到 `model/capabilitiesChanged` 后由宿主刷新清单。

- [ ] **Step 5: 跑测试确认通过**

```bash
cd .worktrees/feat-reasoning-capability/desktop && npx vitest run src/lib/modelPicker.test.ts src/components/SettingsModelsTab.test.tsx src/components/ModelPicker.test.tsx && npx tsc --noEmit
```
Expected: 全绿。

- [ ] **Step 6: 提交**

```bash
cd .worktrees/feat-reasoning-capability && git add -A && git commit -m "feat(desktop): effort 下拉只列模型支持的档位"
```

---

### Task 8: TUI 的 `/effort` 校验与状态行

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/slash.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:761`
- Test: `yi-agent-rs/crates/yi-agent/src/tui/slash.rs`（同文件测试）

**Interfaces:**
- Consumes: Task 6 的 `supported_efforts`
- Produces: `pub fn parse_effort_args_with_caps(args: &str, supported: Option<&[String]>) -> Result<EffortAction, String>`

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn effort_rejects_a_tier_the_model_does_not_support() {
    let supported = vec!["low".to_string(), "high".to_string()];
    let err = parse_effort_args_with_caps("max", Some(&supported)).unwrap_err();
    assert!(err.contains("low"), "error must list the supported tiers: {err}");
    assert!(parse_effort_args_with_caps("high", Some(&supported)).is_ok());
    // 能力未知时行为不变。
    assert!(parse_effort_args_with_caps("max", None).is_ok());
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd .worktrees/feat-reasoning-capability/yi-agent-rs && cargo test -p yi-agent --lib effort_rejects_a_tier
```
Expected: FAIL。

- [ ] **Step 3: 实现**

```rust
/// 与 [`parse_effort_args`] 相同，但额外按模型能力集合校验档位。
///
/// `supported = None` 表示能力未知：沿用七档白名单（零回归）。
pub fn parse_effort_args_with_caps(
    args: &str,
    supported: Option<&[String]>,
) -> Result<EffortAction, String> {
    let action = parse_effort_args(args)?;
    if let (EffortAction::Set(tier), Some(list)) = (&action, supported) {
        if !list.iter().any(|s| s == tier) {
            return Err(format!(
                "该模型不支持 {tier}（支持：{}）",
                if list.is_empty() {
                    "无".to_string()
                } else {
                    list.join("/")
                }
            ));
        }
    }
    Ok(action)
}
```

`app.rs` 的调用点改为先用 `models::supported_efforts(entry)` 取集合（条目选择
复用现有 `effective_entry_with_injection`），再调 `parse_effort_args_with_caps`。

- [ ] **Step 4: 跑测试确认通过**

```bash
cd .worktrees/feat-reasoning-capability/yi-agent-rs && cargo test -p yi-agent --lib effort
```
Expected: 全绿。

- [ ] **Step 5: 提交**

```bash
cd .worktrees/feat-reasoning-capability && cargo fmt --all --manifest-path yi-agent-rs/Cargo.toml && git add -A && git commit -m "feat(tui): /effort 按模型能力集合校验"
```

---

### Task 9: 文档与进度

**Files:**
- Modify: `docs/project-management/yi-agent-llm.md`
- Modify: `docs/project-management/yi-agent-runtime.md`
- Modify: `docs/project-management/yi-agent-app-server.md`
- Modify: `docs/project-management/desktop.md`
- Modify: `docs/project-management/README.md`（若计数变化）

- [ ] **Step 1: 按三态规则登记**

每个模块文件加一条 `[x]`，并写可验证判据（文件 + 行或命令）。例如：

```markdown
- [x] effort 能力探测：`yi-agent-rs/crates/yi-agent-llm/src/probe.rs` 的
  `probe_effort`（裸 HTTP，绕开剥离重投）；验证：`cargo test -p yi-agent-llm --lib probe::tests`
- [x] 能力落盘：`yi-agent-runtime/src/models.rs` 的 `ReasoningCapabilities`；
  验证：`cargo test -p yi-agent-runtime --lib reasoning_capabilities`
```

- [ ] **Step 2: 更新 README 计数**

按 `docs/project-management/README.md` 的索引表更新「完成 / 总计」。

- [ ] **Step 3: 提交**

```bash
cd .worktrees/feat-reasoning-capability && git add -A && git commit -m "docs: 登记 effort 能力探测的实现状态"
```

---

## Self-Review

**Spec coverage**

| spec 章节 | 覆盖任务 |
|---|---|
| §4 数据模型 | Task 1 |
| §4.3 写路径保留/清空 | Task 2 |
| §5.2/5.3 裸 HTTP 探测 | Task 3 |
| §5.4 通道解析 | Task 3（`detect_openai_channel`）+ Task 4（编排） |
| §5.5 探测序列（基线 + 7 档） | Task 4 |
| §5.6 结果分类 | Task 3 |
| §5.7 时序与预算 | Task 4 |
| §5.8 写回防覆盖 | Task 5（`apply_probe_result`） |
| §6.1 请求期过滤 | Task 6 |
| §6.2 写入期校验 | Task 6 |
| §7.1 `model/list` 字段 | Task 5 |
| §7.2 `model/probe` | Task 5 |
| §7.3 `model/capabilitiesChanged` | Task 5 |
| §8.1 设置页 | Task 7 |
| §8.2 会话控件 | Task 7 |
| §8.3 TUI | Task 8 |
| §10 测试策略 | 各任务 Step 1/2/4 |
| §11 落地顺序 | Task 1–9 顺序一致 |

**已知的实现期细节（执行者需要现场确认）**

1. `OpenaiRequest` / `responses_request_from` / `AnthropicRequest` 的可见性：若当前
   私有，提升为 `pub(crate)`；不改序列化逻辑。
2. `AppState` 新增 `probes` 字段会影响 `serve()` 里所有 `AppState` 构造点，编译器
   会逐一指出。
3. `rpc_tests.rs` 的两个集成用例需要照抄相邻 `model/*` 用例的 harness，不要新造。
4. `RuntimeConfig` **没有** `Default` 实现：测试配置按 `model_rpc.rs:369` 的
   `fallback_cfg()` 逐字段构造，不要新增 `Default`。
5. 桌面端 `model/capabilitiesChanged` 的消费点在 `App.tsx` 的通知分发处；新增分支
   只需把新能力并回本地 `catalog` 状态，不重拉全量。
6. `Notification` 新增变体后必须跑 `cargo test -p yi-agent-app-server export_bindings`
   重新生成 `desktop/src/lib/generated/Notification.ts` 并提交，否则 `just check-ts` 红。
7. `ModelEntry` 新增字段会让全部构造点编译失败；用
   `grep -rn "ModelEntry {" yi-agent-rs/crates --include=*.rs` 找齐后统一补
   `reasoning_capabilities: None,`。
