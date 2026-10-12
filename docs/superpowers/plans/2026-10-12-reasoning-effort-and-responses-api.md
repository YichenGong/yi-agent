# Reasoning Effort + Responses API Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让模型条目/会话可配 `reasoning_effort`，给 openai 通道增加 `/v1/responses` 支持并在 `auto` 下自动探测+透明回退，同时规范化展示 CoT。

**Architecture:** 三层扩展：(1) 配置层给 `models.json` 条目与 thread meta 加 `reasoning_effort` / `api_style` 字段，经 `resolve_effective` 进入 `RuntimeConfig`；(2) provider 层按 `api_style` 分派 chat / responses 两个端点，`auto` 首次探测网关能力并缓存，被网关拒绝的 effort 档自动剥离重投；(3) 展示层用一个统一的 `ProviderEvent::ReasoningDelta` 承载两个端点的 CoT，三端各自渲染为可折叠区。

**Tech Stack:** Rust（`yi-agent-core` / `yi-agent-runtime` / `yi-agent-llm` / `yi-agent-app-server` / `yi-agent`）、wiremock（provider 测试）、React + TypeScript + vitest（desktop）。

设计依据：`docs/superpowers/specs/2026-10-12-reasoning-effort-and-responses-api-design.md`。

## Global Constraints

- 不在 `main` 分支直接改动；所有实现走 worktree（`.worktrees/reasoning-effort`），分支 `docs/reasoning-effort-responses-design` 已存在，实现阶段可另建 `feat/reasoning-effort` 分支。
- 提交前在 `yi-agent-rs/` 跑 `cargo fmt --all`；commit message 用 conventional commits，首行 ≤72 字符，**不写** `Co-Authored-By`。
- 跑测试按 crate 进行（`cargo test -p <crate>`），避免 `--workspace` 全量；跑前 `ps aux | grep cargo` 确认无残留 cargo 进程。
- 后端改动不得让既有测试变红；`cargo test -p yi-agent-llm`、`-p yi-agent-core`、`-p yi-agent-runtime`、`-p yi-agent-app-server`、`-p yi-agent` 均须通过。
- 前端：`cd desktop && npx vitest run && npx tsc --noEmit && npm run build` 均须通过。
- **零回归硬要求**：`reasoning_effort` 未配置时，chat 请求体与今天**逐字节相同**；`api_style` 未配置时默认 `auto`。
- effort 档位枚举（固定）：`none | minimal | low | medium | high | xhigh | max`。**未设置 = 不发该字段**。
- worktree 内所有 `cargo`/`npm` 命令用 worktree 自己的目录（`<worktree>/yi-agent-rs`、`<worktree>/desktop`）。

---

## File Structure

**新增**
- `yi-agent-rs/crates/yi-agent-llm/src/openai/responses/mod.rs` — responses 模块入口
- `yi-agent-rs/crates/yi-agent-llm/src/openai/responses/types.rs` — responses 请求体构造
- `yi-agent-rs/crates/yi-agent-llm/src/openai/responses/stream.rs` — responses SSE 解析
- `yi-agent-rs/crates/yi-agent-llm/src/openai/channel.rs` — `ApiStyle` / `Channel` 与探测缓存
- `yi-agent-rs/crates/yi-agent-llm/tests/openai_responses.rs` — responses 通道集成测试（wiremock）

**修改**
- `yi-agent-rs/crates/yi-agent-core/src/provider.rs` — `GenParams` / `ProviderEvent` / `TokenUsage`
- `yi-agent-rs/crates/yi-agent-runtime/src/models.rs` — `ModelEntry` 新字段、`resolve_effective` 带出新字段
- `yi-agent-rs/crates/yi-agent-runtime/src/config.rs` — `RuntimeConfig` 新字段
- `yi-agent-rs/crates/yi-agent-runtime/src/bootstrap.rs` — `build_provider` 传新字段
- `yi-agent-rs/crates/yi-agent-runtime/src/onboarding.rs` — 探测请求构造点同步
- `yi-agent-rs/crates/yi-agent-llm/src/openai/types.rs` — chat 请求带 `reasoning_effort`
- `yi-agent-rs/crates/yi-agent-llm/src/openai/client.rs` — 通道分派、探测、回退、剥离重投
- `yi-agent-rs/crates/yi-agent-llm/src/openai/error.rs` — 识别 `reasoning_effort`/`reasoning.effort` 类错误
- `yi-agent-rs/crates/yi-agent-llm/src/openai/mod.rs` — 挂 `channel` / `responses`
- `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs` — meta 新字段 + `set_reasoning_effort`
- `yi-agent-rs/crates/yi-agent-app-server/src/model_rpc.rs` — 条目视图带新字段、upsert 接受新字段
- `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` — `thread/setReasoningEffort` RPC + 广播
- `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs` — 新方法与通知名
- `yi-agent-rs/crates/yi-agent/src/tui/app.rs` — `/effort` 命令
- `yi-agent-rs/crates/yi-agent/src/tui/slash.rs` — 命令注册
- `yi-agent-rs/crates/yi-agent/src/tui/history.rs` — 推理块渲染
- `desktop/src/lib/models.ts` — 条目视图/upsert 类型带新字段
- `desktop/src/lib/modelPicker.ts` — effort 选项构造（纯逻辑）
- `desktop/src/components/SettingsModelsTab.tsx` — effort 字段 + 高级折叠区
- `desktop/src/components/ModelPicker.tsx` — 主界面 effort 控件

**依赖顺序**：Task 1 → 2 → 3 → 4 → 5 → 6 → 7 → 8 → 9。Task 1 是所有后续的类型基础。

---

### Task 1: Core 类型扩展（`GenParams` / `ProviderEvent` / `TokenUsage`）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/provider.rs:23-88`

**Interfaces:**
- Produces:
  - `GenParams.reasoning_effort: Option<String>`
  - `TokenUsage.reasoning_tokens: Option<u32>`
  - `ProviderEvent::ReasoningDelta { text: String, kind: ReasoningKind }`
  - `ProviderEvent::Notice { level: NoticeLevel, text: String }`
  - `enum ReasoningKind { Content, Summary }`
  - `enum NoticeLevel { Info, Warn }`
  - `enum ApiStyle { Auto, Responses, Chat }` + `ApiStyle::parse(&str) -> Self` +
    `ApiStyle::as_str(&self) -> &'static str`（**唯一定义处**；runtime 与 llm 都引用它，
    避免两份实现漂移）

- [ ] **Step 1: Write the failing test**

在 `provider.rs` 的 `mod tests` 内追加：

```rust
#[test]
fn gen_params_defaults_to_no_reasoning_effort() {
    let p = GenParams::default();
    assert_eq!(p.reasoning_effort, None);
}

#[test]
fn reasoning_and_notice_events_are_constructible() {
    let a = ProviderEvent::ReasoningDelta {
        text: "thinking".into(),
        kind: ReasoningKind::Content,
    };
    let b = ProviderEvent::ReasoningDelta {
        text: "summary".into(),
        kind: ReasoningKind::Summary,
    };
    let c = ProviderEvent::Notice {
        level: NoticeLevel::Info,
        text: "fell back to chat".into(),
    };
    assert_ne!(a, b);
    assert!(matches!(c, ProviderEvent::Notice { .. }));
}

#[test]
fn token_usage_carries_reasoning_tokens() {
    let u = TokenUsage {
        input_tokens: 1,
        output_tokens: 2,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
        reasoning_tokens: Some(7),
    };
    assert_eq!(u.reasoning_tokens, Some(7));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib provider::tests`
Expected: 编译失败（`reasoning_effort` / `ReasoningDelta` / `reasoning_tokens` 未定义）。

- [ ] **Step 3: Write minimal implementation**

在 `GenParams` 加字段：

```rust
pub struct GenParams {
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub top_p: Option<f32>,
    pub stop_sequences: Option<Vec<String>>,
    /// `reasoning_effort` 档位；`None` = 不发该字段（零回归默认）。
    pub reasoning_effort: Option<String>,
}
```

`TokenUsage` 加字段：

```rust
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_creation_input_tokens: Option<u32>,
    pub cache_read_input_tokens: Option<u32>,
    /// 推理模型消耗的 reasoning token（responses 提供；chat 常为 None）。
    pub reasoning_tokens: Option<u32>,
}
```

在 `ProviderEvent` 加两个变体与三个枚举：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningKind {
    /// 完整思考正文（DeepSeek 的 reasoning_text）。
    Content,
    /// 结构化摘要（GPT-6 的 reasoning_summary_text）。
    Summary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeLevel {
    Info,
    Warn,
}

/// 端点风格。解析非法值落回 `Auto`，绝不因一个手写值让条目失效。
/// 唯一定义处：runtime 的 `RuntimeConfig` 与 llm 的 provider 都引用本类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiStyle {
    Auto,
    Responses,
    Chat,
}

impl ApiStyle {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "responses" => Self::Responses,
            "chat" => Self::Chat,
            _ => Self::Auto,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Responses => "responses",
            Self::Chat => "chat",
        }
    }
}

pub enum ProviderEvent {
    TextDelta(String),
    ToolUseStart { id: String, name: String },
    ToolUseDelta { id: String, partial_json: String },
    ToolUseEnd { id: String },
    /// 归一化的思维链增量；两个端点都映射到它。
    ReasoningDelta { text: String, kind: ReasoningKind },
    Stop { reason: StopReason },
    StreamError(ProviderError),
    Usage(TokenUsage),
    /// 非致命的运行期告知（回退/降级），展示在状态栏级。
    Notice { level: NoticeLevel, text: String },
}
```

`ApiStyle` 必须从 `yi-agent-core` 导出（`lib.rs` 的 `pub use provider::{…, ApiStyle, ReasoningKind, NoticeLevel}`），供 `yi-agent-runtime` 与 `yi-agent-llm` 共用。

`TokenUsage` 现在多一个字段，找到所有构造点补 `reasoning_tokens: None`：

Run: `cd yi-agent-rs && cargo build -p yi-agent-core 2>&1 | grep -A2 'missing field'`
逐个补齐（`openai/stream.rs` 两个 `TokenUsage {` 构造点补 `reasoning_tokens: None`）。

- [ ] **Step 4: Run test to verify it passes**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib provider::tests`
Expected: PASS。

- [ ] **Step 5: Fix downstream breakage**

Run: `cd yi-agent-rs && cargo test -p yi-agent-llm -p yi-agent-core --lib`
Expected: 现有测试仍全绿（新字段都有默认/补齐）。修好任何 `missing field` 编译错误。

- [ ] **Step 6: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-core/src/provider.rs crates/yi-agent-llm/src/openai/stream.rs
git commit -m "feat(core): add reasoning effort param, reasoning/notice events"
```

---

### Task 2: 配置层贯穿（条目字段 → RuntimeConfig）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/models.rs:40-49,207-236`
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/config.rs:22-50,359-378,405-426`
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/bootstrap.rs:11-32`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/model_rpc.rs`（条目视图 + upsert）
- Test: `yi-agent-rs/crates/yi-agent-runtime/src/models.rs` 内 `mod tests` / `mod resolve_tests`

**Interfaces:**
- Consumes: Task 1 无直接依赖。
- Produces:
  - `ModelEntry.api_style: Option<String>`（`#[serde(default)]`）
  - `ModelEntry.reasoning_effort: Option<String>`（`#[serde(default)]`）
  - `RuntimeConfig.api_style: ApiStyle`（枚举，见下）与 `RuntimeConfig.reasoning_effort: Option<String>`
  - `RuntimeConfig::default_api_style()` 常量默认 `Auto`

- [ ] **Step 1: Write the failing test**

在 `models.rs` 的 `mod tests` 内追加：

```rust
#[test]
fn entries_round_trip_the_two_new_optional_fields() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("models.json");
    std::fs::write(
        &path,
        r#"{"models":[{"name":"L","provider":"openai","api_url":"u","model":"m",
            "api_key":"k","api_style":"responses","reasoning_effort":"none"}]}"#,
    )
    .unwrap();
    let catalog = load_catalog_from(&path);
    assert_eq!(catalog.models.len(), 1);
    assert_eq!(catalog.models[0].api_style.as_deref(), Some("responses"));
    assert_eq!(catalog.models[0].reasoning_effort.as_deref(), Some("none"));
}

#[test]
fn legacy_entries_without_the_new_fields_still_load() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("models.json");
    std::fs::write(
        &path,
        r#"{"models":[{"name":"A","provider":"openai","api_url":"u","model":"m","api_key":"k"}]}"#,
    )
    .unwrap();
    let catalog = load_catalog_from(&path);
    assert_eq!(catalog.models.len(), 1, "missing optional fields must not skip the entry");
    assert_eq!(catalog.models[0].api_style, None);
    assert_eq!(catalog.models[0].reasoning_effort, None);
}
```

在 `mod resolve_tests` 内追加：

```rust
#[test]
fn resolve_effective_carries_api_style_and_effort_from_the_entry() {
    let mut cat = catalog();
    cat.models[1].api_style = Some("responses".into());
    cat.models[1].reasoning_effort = Some("low".into());
    let out = resolve_effective(&cfg(), &cat, Some("B"));
    assert_eq!(out.reasoning_effort.as_deref(), Some("low"));
    assert_eq!(out.api_style.as_str(), "responses");
}

#[test]
fn resolve_effective_defaults_to_auto_and_no_effort_when_entry_silent() {
    let out = resolve_effective(&cfg(), &catalog(), Some("A"));
    assert_eq!(out.reasoning_effort, None);
    assert_eq!(out.api_style.as_str(), "auto");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime --lib models::`
Expected: 编译失败（字段/枚举不存在）。

- [ ] **Step 3: Implement**

`models.rs` — `ModelEntry` 加字段：

```rust
pub struct ModelEntry {
    pub name: String,
    #[serde(deserialize_with = "de_provider")]
    pub provider: ModelProvider,
    pub api_url: String,
    pub model: String,
    #[serde(default)]
    pub api_key: String,
    /// `auto`（默认）| `responses` | `chat`；未知值按 `auto` 处理。
    #[serde(default)]
    pub api_style: Option<String>,
    /// 七档之一；`None` = 不发该字段。
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}
```

`config.rs` — **不重新定义枚举**，直接引用 core 的 `ApiStyle`（Task 1 产出，单一来源）：

```rust
use yi_agent_core::ApiStyle;
```

`RuntimeConfig` 加两个字段（放在 `model` 之后）：

```rust
pub api_style: ApiStyle,
pub reasoning_effort: Option<String>,
```

`load_inner` 末尾构造时给默认值：`api_style: ApiStyle::Auto, reasoning_effort: None,`。
`sample_config()`（`config.rs:405`）与 `models.rs` 的 `resolve_tests::cfg()`、`config.rs` 内所有字面量 `RuntimeConfig {` 构造点补这两个字段。

`models.rs` — `resolve_effective` 带出新字段：

```rust
Some(entry) => {
    let mut out = cfg.clone();
    out.provider = entry.provider.as_str().to_string();
    out.api_url = entry.api_url.clone();
    out.api_key = entry.api_key.clone();
    out.model = entry.model.clone();
    out.api_style = entry
        .api_style
        .as_deref()
        .map(ApiStyle::parse)
        .unwrap_or(ApiStyle::Auto);
    out.reasoning_effort = entry.reasoning_effort.clone();
    out
}
```

`bootstrap.rs` — `build_provider` 把两字段传给 provider opts（Task 3 定义 opts 字段；本步先改为引用 `cfg.api_style.as_str()` 与 `cfg.reasoning_effort.clone()`，若 Task 3 未完成则暂不编译——**执行顺序要求 Task 3 紧随**；如单独跑本任务，先只加字段、`build_provider` 留到 Task 3）。

`model_rpc.rs` — `entry_view` 增加两键；`upsert` 读取并校验：

```rust
// entry_view 内追加
"api_style": entry.api_style,
"reasoning_effort": entry.reasoning_effort,
```

```rust
// upsert 内，构造 ModelEntry 前：
let api_style = optional_string_field(params, "api_style");
if let Some(v) = &api_style {
    if !matches!(v.as_str(), "auto" | "responses" | "chat") {
        return Err(RpcError::invalid_model("api_style must be auto|responses|chat"));
    }
}
let reasoning_effort = optional_string_field(params, "reasoning_effort");
if let Some(v) = &reasoning_effort {
    if !matches!(v.as_str(), "none"|"minimal"|"low"|"medium"|"high"|"xhigh"|"max") {
        return Err(RpcError::invalid_model("unknown reasoning_effort"));
    }
}
```

补 `fn optional_string_field(params: &Value, key: &str) -> Option<String>`（缺失/null → None；字符串 → Some；其它 → None）。

- [ ] **Step 4: Run test to verify it passes**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime --lib`
Expected: PASS（含新增 4 条）。

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-runtime/src crates/yi-agent-app-server/src/model_rpc.rs
git commit -m "feat(runtime): carry api_style and reasoning_effort through config"
```

---

### Task 3: Provider opts + chat 通道带 effort

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/openai/client.rs:29-93`
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/openai/types.rs:78-88,309-314`
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/bootstrap.rs:11-32`
- Test: `yi-agent-rs/crates/yi-agent-llm/tests/openai_integration.rs`

**Interfaces:**
- Consumes: Task 1 `GenParams.reasoning_effort`；Task 2 `RuntimeConfig.api_style/reasoning_effort`。
- Produces: `OpenaiProviderOpts { base_url, api_key, timeout, api_style: Option<String>, reasoning_effort: Option<String> }`。

- [ ] **Step 1: Write the failing test**

在 `openai_integration.rs` 追加：

```rust
#[tokio::test]
async fn chat_request_carries_reasoning_effort_when_set() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(serde_json::json!({"reasoning_effort": "none"})))
        .respond_with(ResponseTemplate::new(200).set_body_string(SSE_TEXT_ONLY))
        .mount(&server)
        .await;
    let provider = OpenaiProvider::new(OpenaiProviderOpts {
        base_url: Some(server.uri()),
        api_key: Some("k".into()),
        timeout: Some(Duration::from_secs(5)),
        api_style: Some("chat".into()),
        reasoning_effort: Some("none".into()),
    })
    .unwrap();
    let mut req = simple_request();
    req.params.reasoning_effort = Some("none".into());
    let _ = provider.call(req).await.expect("call ok");
}

#[tokio::test]
async fn chat_request_omits_reasoning_effort_when_unset() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        // 断言请求体里根本没有 reasoning_effort 键
        .and(|req: &wiremock::Request| {
            !serde_json::from_slice::<serde_json::Value>(&req.body)
                .map(|v| v.get("reasoning_effort").is_some())
                .unwrap_or(true)
        })
        .respond_with(ResponseTemplate::new(200).set_body_string(SSE_TEXT_ONLY))
        .mount(&server)
        .await;
    let provider = OpenaiProvider::new(OpenaiProviderOpts {
        base_url: Some(server.uri()),
        api_key: Some("k".into()),
        timeout: Some(Duration::from_secs(5)),
        api_style: Some("chat".into()),
        reasoning_effort: None,
    })
    .unwrap();
    let _ = provider.call(simple_request()).await.expect("call ok");
}
```

（`SSE_TEXT_ONLY` 是文件内既有的最小 SSE 常量；若不存在，复用其它用例里的 body 字符串。）

- [ ] **Step 2: Run test to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent-llm --test openai_integration chat_request_`
Expected: 编译失败（opts 无新字段）。

- [ ] **Step 3: Implement**

`types.rs` — `OpenaiGenParams` 加字段并在 `From<ProviderRequest>` 里透传：

```rust
pub struct OpenaiGenParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "stop")]
    pub stop_sequences: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}
```

`From<ProviderRequest>` 内 `OpenaiGenParams { …, reasoning_effort: req.params.reasoning_effort.clone() }`。

`client.rs` — `OpenaiProviderOpts` 加字段、`OpenaiProvider` 保存两字段：

```rust
#[derive(Default)]
pub struct OpenaiProviderOpts {
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub timeout: Option<Duration>,
    /// `auto`（默认）| `responses` | `chat`。
    pub api_style: Option<String>,
    /// 七档之一；`None` = 不发。
    pub reasoning_effort: Option<String>,
}
```

`OpenaiProvider` 加 `api_style: crate::openai::channel::ApiStyle`、`reasoning_effort: Option<String>`、`channel_cache: Arc<Mutex<HashMap<String, Channel>>>`（Task 5 用；本步先存 `api_style`/`reasoning_effort`，`channel_cache` 可延后到 Task 5）。

`bootstrap.rs` — `build_provider` 的 openai 分支：

```rust
"openai" => Ok(Arc::new(yi_agent_llm::OpenaiProvider::new(
    yi_agent_llm::OpenaiProviderOpts {
        base_url: Some(cfg.api_url.clone()),
        api_key: Some(cfg.api_key.clone()),
        api_style: Some(cfg.api_style.as_str().to_string()),
        reasoning_effort: cfg.reasoning_effort.clone(),
        ..Default::default()
    },
)?)),
```

同时把 provider 构造时把 `req.params.reasoning_effort` 复制进请求：在 `call_stream` 里若 `self.reasoning_effort.is_some()` 且 `req.params.reasoning_effort.is_none()`，用 provider 级值覆盖（provider 级是 resolve 后的权威值）。

`onboarding.rs:305` 的 `OpenaiProviderOpts { … }` 用 `..Default::default()` 已兼容，确认无需改。

- [ ] **Step 4: Run test to verify it passes**

Run: `cd yi-agent-rs && cargo test -p yi-agent-llm --test openai_integration`
Expected: PASS，且既有 chat 用例全绿。

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-llm/src crates/yi-agent-runtime/src/bootstrap.rs
git commit -m "feat(llm): chat channel carries reasoning_effort"
```

---

### Task 4: responses 通道（请求体 + SSE 解析）

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-llm/src/openai/responses/mod.rs`
- Create: `yi-agent-rs/crates/yi-agent-llm/src/openai/responses/types.rs`
- Create: `yi-agent-rs/crates/yi-agent-llm/src/openai/responses/stream.rs`
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/openai/mod.rs`
- Test: `yi-agent-rs/crates/yi-agent-llm/src/openai/responses/stream.rs` 内 `mod tests`

**Interfaces:**
- Consumes: Task 1 的 `ProviderEvent` / `ReasoningKind` / `TokenUsage.reasoning_tokens`。
- Produces:
  - `responses::types::ResponsesRequest`（`Serialize`）
  - `responses::types::responses_request_from(req: ProviderRequest) -> ResponsesRequest`
  - `responses::stream::ResponsesStream<S>`（`Stream<Item = Result<ProviderEvent, ProviderError>>`）

- [ ] **Step 1: Write the failing test**

在 `responses/stream.rs` 的 `mod tests`：

```rust
#[tokio::test]
async fn maps_output_text_and_reasoning_deltas() {
    let body = "event: response.output_text.delta\n\
        data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\"}\n\n\
        event: response.reasoning_text.delta\n\
        data: {\"type\":\"response.reasoning_text.delta\",\"delta\":\"think\"}\n\n\
        event: response.reasoning_summary_text.delta\n\
        data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"sum\"}\n\n\
        event: response.completed\n\
        data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":4,\"output_tokens_details\":{\"reasoning_tokens\":9}}}}\n\n";
    let events = collect(body).await;
    assert!(matches!(&events[0], ProviderEvent::TextDelta(t) if t == "Hi"));
    assert!(matches!(&events[1], ProviderEvent::ReasoningDelta{ text, kind: ReasoningKind::Content } if text == "think"));
    assert!(matches!(&events[2], ProviderEvent::ReasoningDelta{ text, kind: ReasoningKind::Summary } if text == "sum"));
    assert!(matches!(events.last(), Some(ProviderEvent::Stop { .. })));
    assert!(events.iter().any(|e| matches!(e, ProviderEvent::Usage(u) if u.reasoning_tokens == Some(9))));
}

#[tokio::test]
async fn maps_function_call_lifecycle() {
    let body = "event: response.output_item.added\n\
        data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"ls\",\"arguments\":\"\"}}\n\n\
        event: response.function_call_arguments.delta\n\
        data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"delta\":\"{}\"}\n\n\
        event: response.output_item.done\n\
        data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"ls\",\"arguments\":\"{}\"}}\n\n\
        event: response.completed\n\
        data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n";
    let events = collect(body).await;
    assert!(matches!(&events[0], ProviderEvent::ToolUseStart{ id, name } if id == "call_1" && name == "ls"));
    assert!(matches!(&events[1], ProviderEvent::ToolUseDelta{ id, partial_json } if id == "call_1" && partial_json == "{}"));
    assert!(matches!(&events[2], ProviderEvent::ToolUseEnd{ id } if id == "call_1"));
}

#[test]
fn request_builds_instructions_input_tools_and_reasoning() {
    let req = ProviderRequest {
        model: "HORIZON-OG-Luna".into(),
        system: Some("be brief".into()),
        messages: vec![Message::user("hi")],
        tools: vec![ToolSchema { name: "ls".into(), description: "list".into(), input_schema: serde_json::json!({"type":"object"}) }],
        params: GenParams { reasoning_effort: Some("low".into()), ..Default::default() },
    };
    let body = serde_json::to_value(responses_request_from(req)).unwrap();
    assert_eq!(body["instructions"], "be brief");
    assert_eq!(body["reasoning"]["effort"], "low");
    assert_eq!(body["reasoning"]["summary"], "auto");
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["name"], "ls");
    assert!(body["tools"][0].get("function").is_none(), "responses tools 无 function 包裹");
    assert_eq!(body["input"][0]["role"], "user");
}

#[tokio::test]
async fn request_omits_reasoning_block_when_effort_unset() {
    let req = ProviderRequest {
        model: "m".into(), system: None,
        messages: vec![Message::user("hi")], tools: vec![],
        params: GenParams::default(),
    };
    let body = serde_json::to_value(responses_request_from(req)).unwrap();
    assert!(body.get("reasoning").is_none());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent-llm --lib openai::responses`
Expected: 编译失败（模块不存在）。

- [ ] **Step 3: Implement types.rs**

```rust
//! /v1/responses 请求体（无状态重放；实测免 previous_response_id 可做工具往返）。

use serde::Serialize;
use serde_json::Value;
use yi_agent_core::{ContentBlock, ImageSource, ProviderRequest};

#[derive(Serialize)]
pub struct ResponsesRequest {
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    pub input: Vec<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Reasoning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    pub stream: bool,
}

#[derive(Serialize)]
pub struct Reasoning {
    pub effort: String,
    /// 固定 auto —— 实测只有它能让 GPT-6 返回明文摘要。
    pub summary: String,
}

pub fn responses_request_from(req: ProviderRequest) -> ResponsesRequest {
    let mut input: Vec<Value> = Vec::new();
    for m in &req.messages {
        match m.role {
            Role::User => input.push(user_item(&m.content)),
            Role::Assistant => {
                let text: String = m.content.iter().filter_map(|b| match b {
                    ContentBlock::Text(t) => Some(t.as_str()), _ => None }).collect();
                if !text.is_empty() {
                    input.push(serde_json::json!({"role":"assistant","content":[{"type":"output_text","text":text}]}));
                }
                for b in &m.content {
                    if let ContentBlock::ToolUse { id, name, input: args } = b {
                        input.push(serde_json::json!({"type":"function_call","call_id":id,"name":name,"arguments":args.to_string()}));
                    }
                }
            }
            Role::Tool => {
                for b in &m.content {
                    if let ContentBlock::ToolResult { tool_use_id, content, is_error } = b {
                        let text = content.iter().filter_map(|c| match c {
                            ContentBlock::Text(t) => Some(t.as_str()), _ => None }).collect::<Vec<_>>().join("");
                        let out = if *is_error { format!("error: {text}") } else { text };
                        input.push(serde_json::json!({"type":"function_call_output","call_id":tool_use_id,"output":out}));
                    }
                }
            }
            Role::System => { /* system 走 instructions */ }
        }
    }
    let tools = req.tools.into_iter().map(|t| serde_json::json!({
        "type": "function", "name": t.name, "description": t.description, "parameters": t.input_schema
    })).collect();
    let reasoning = req.params.reasoning_effort.map(|effort| Reasoning { effort, summary: "auto".into() });
    ResponsesRequest {
        model: req.model,
        instructions: req.system,
        input,
        tools,
        reasoning,
        temperature: req.params.temperature,
        top_p: req.params.top_p,
        max_output_tokens: req.params.max_tokens,
        stream: true,
    }
}

fn user_item(blocks: &[ContentBlock]) -> Value {
    let has_image = blocks.iter().any(|b| matches!(b, ContentBlock::Image { .. }));
    if !has_image {
        let text: String = blocks.iter().filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.as_str()), _ => None }).collect();
        return serde_json::json!({"role":"user","content":[{"type":"input_text","text":text}]});
    }
    let mut parts: Vec<Value> = Vec::new();
    let text: String = blocks.iter().filter_map(|b| match b {
        ContentBlock::Text(t) => Some(t.as_str()), _ => None }).collect();
    if !text.is_empty() { parts.push(serde_json::json!({"type":"input_text","text":text})); }
    for b in blocks {
        if let ContentBlock::Image { source, .. } = b {
            let url = match source {
                ImageSource::Base64 { media_type, data } => format!("data:{media_type};base64,{data}"),
                ImageSource::Url(u) => u.clone(),
            };
            parts.push(serde_json::json!({"type":"input_image","image_url":url}));
        }
    }
    serde_json::json!({"role":"user","content":parts})
}
```

（`Role` 从 `yi_agent_core::Role` 引入。）

- [ ] **Step 4: Implement stream.rs**

按 SSE 行解析（复用 `openai/stream.rs` 的 `SseLineParser` 思路，抽成同形私有结构），对每帧取 `data.type` 分派：

```rust
match ty {
    "response.output_text.delta" => events.push(TextDelta(delta)),
    "response.reasoning_text.delta" => events.push(ReasoningDelta { text: delta, kind: ReasoningKind::Content }),
    "response.reasoning_summary_text.delta" => events.push(ReasoningDelta { text: delta, kind: ReasoningKind::Summary }),
    "response.output_item.added" => if item.type=="function_call" { push ToolUseStart { id: item.call_id, name: item.name } },
    "response.function_call_arguments.delta" => push ToolUseDelta { id: <call_id 由 item_id→call_id 映射>, partial_json: delta },
    "response.output_item.done" => if function_call { push ToolUseEnd { id: call_id } },
    "response.completed" => { push Usage(from response.usage; reasoning_tokens from output_tokens_details.reasoning_tokens); push Stop { EndTurn } }
    _ => {} // created / in_progress / content_part.* 等忽略
}
```

维护 `item_id -> call_id` 映射（`output_item.added` 时插入）。`function_call_arguments.delta` 只带 `item_id`，需查表得 `call_id`。

- [ ] **Step 5: Wire mod.rs**

`openai/mod.rs` 加 `pub mod channel;`（Task 5）与 `pub mod responses;`。本步先加 `pub mod responses;`。

- [ ] **Step 6: Run test to verify it passes**

Run: `cd yi-agent-rs && cargo test -p yi-agent-llm --lib openai::responses`
Expected: PASS。

- [ ] **Step 7: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-llm/src/openai/responses crates/yi-agent-llm/src/openai/mod.rs
git commit -m "feat(llm): add /v1/responses request builder and SSE parser"
```

---

### Task 5: 通道分派 + auto 探测 + 透明回退

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-llm/src/openai/channel.rs`
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/openai/client.rs:95-155`
- Test: `yi-agent-rs/crates/yi-agent-llm/tests/openai_responses.rs`（新建）

**Interfaces:**
- Consumes: Task 4 的 `responses_request_from` / `ResponsesStream`；Task 3 的 opts。
- Produces:
  - `channel::Channel { Chat, Responses }`
  - `channel::looks_like_missing_route(status, body) -> bool`
  - `channel::ChannelCache = Arc<Mutex<HashMap<String, Channel>>>`
  - `OpenaiProvider.channel_cache: ChannelCache`
  - （`ApiStyle` **不从本模块定义**，用 `yi_agent_core::ApiStyle`，Task 1 产出）

- [ ] **Step 1: Write the failing test**

`tests/openai_responses.rs`：

```rust
#[tokio::test]
async fn auto_probe_falls_back_to_chat_when_responses_404s_empty() {
    let server = MockServer::start().await;
    // /v1/responses → 404 空 body（网关不支持）
    Mock::given(method("POST")).and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(404).set_body_string(""))
        .mount(&server).await;
    // chat 正常
    Mock::given(method("POST")).and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SSE_TEXT_ONLY))
        .mount(&server).await;

    let provider = OpenaiProvider::new(OpenaiProviderOpts {
        base_url: Some(server.uri()), api_key: Some("k".into()),
        timeout: Some(Duration::from_secs(5)),
        api_style: Some("auto".into()), reasoning_effort: None,
    }).unwrap();
    let resp = provider.call(simple_request()).await.expect("auto fallback must succeed");
    assert_eq!(resp.stop_reason, StopReason::EndTurn);
    // 至少收到一条 Notice 告知已回退
    assert!(resp.notices.iter().any(|n| n.text.contains("Chat")));
}

#[tokio::test]
async fn auto_probe_uses_responses_when_available() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SSE_RESPONSES_TEXT_ONLY))
        .mount(&server).await;
    let provider = OpenaiProvider::new(OpenaiProviderOpts {
        base_url: Some(server.uri()), api_key: Some("k".into()),
        timeout: Some(Duration::from_secs(5)),
        api_style: Some("auto".into()), reasoning_effort: None,
    }).unwrap();
    let resp = provider.call(simple_request()).await.expect("responses must work");
    assert_eq!(resp.stop_reason, StopReason::EndTurn);
}

#[tokio::test]
async fn explicit_chat_style_never_probes_responses() {
    let server = MockServer::start().await;
    // 只挂 chat；若代码探测 responses 会收到 wiremock 未预期请求（或 404）
    Mock::given(method("POST")).and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SSE_TEXT_ONLY))
        .mount(&server).await;
    let provider = OpenaiProvider::new(OpenaiProviderOpts {
        base_url: Some(server.uri()), api_key: Some("k".into()),
        timeout: Some(Duration::from_secs(5)),
        api_style: Some("chat".into()), reasoning_effort: None,
    }).unwrap();
    let _ = provider.call(simple_request()).await.expect("chat style ok");
    assert_eq!(server.received_requests().await.unwrap().iter()
        .filter(|r| r.url.path() == "/v1/responses").count(), 0);
}
```

注：`ProviderResponse` 需新增 `notices: Vec<Notice>`（本任务在 Task 6 之前先加：`ProviderResponse { content, stop_reason, notices }`，默认空）。`simple_request()` 复用 `openai_integration.rs` 的 helper（复制到本文件或提为 `common`）。

- [ ] **Step 2: Run test to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent-llm --test openai_responses`
Expected: 编译失败（channel 模块/notices 不存在）。

- [ ] **Step 3: Implement channel.rs**

```rust
//! chat / responses 通道选择与网关能力探测（结果按 base_url 缓存）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

// ApiStyle 来自 core（单一来源），此处只定义「实际通道」与探测判定。
use yi_agent_core::ApiStyle;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel { Chat, Responses }

pub type ChannelCache = Arc<Mutex<HashMap<String, Channel>>>;

/// 判定是否「路径不存在」：404/405 且 body 为空。
/// 实测：不存在的路由回 404 空 body；已知端点回 404 会带 JSON（如 model_not_found）。
pub fn looks_like_missing_route(status: u16, body: &str) -> bool {
    (status == 404 || status == 405) && body.trim().is_empty()
}
```

`client.rs` 的 `call_stream` 改为：

```rust
let channel = self.resolve_channel().await?;
let body = match channel {
    Channel::Chat => serde_json::to_value(OpenaiChatRequest::from(req.clone())),
    Channel::Responses => serde_json::to_value(responses_request_from(req.clone())),
};
let path = match channel { Channel::Chat => "/v1/chat/completions", Channel::Responses => "/v1/responses" };
let resp = self.client.post(format!("{}{}", self.base_url, path)).…send().await?;
if !resp.status().is_success() {
    let status = resp.status().as_u16();
    let body_text = resp.text().await.unwrap_or_default();
    // auto 且 responses 被判为「路径不存在」→ 回退 chat 并按当前请求重投
    if channel == Channel::Responses && self.api_style == ApiStyle::Auto && looks_like_missing_route(status, &body_text) {
        self.cache_channel(Channel::Chat);
        return self.call_stream_chat_with_notice(req, "该地址不支持 Responses，已自动改用 Chat").await;
    }
    return Err(map_status_error_from(status, body_text));
}
```

`resolve_channel`：

```rust
async fn resolve_channel(&self) -> Result<Channel, ProviderError> {
    match self.api_style {
        ApiStyle::Chat => Ok(Channel::Chat),
        ApiStyle::Responses => Ok(Channel::Responses),
        ApiStyle::Auto => {
            if let Some(c) = self.cache.get(&self.base_url) { return Ok(*c); }
            // 探测：用 GET 判路径是否存在（405=存在，404=不存在），失败不致命，默认 Responses
            let probe = self.client.get(format!("{}/v1/responses", self.base_url))
                .header("Authorization", format!("Bearer {}", self.api_key)).send().await;
            let channel = match probe {
                Ok(r) if r.status().as_u16() == 404 => Channel::Chat,
                _ => Channel::Responses,
            };
            self.cache_channel(channel);
            Ok(channel)
        }
    }
}
```

（探测用 `GET`：实测存在→405、不存在→404。GET 不会真正生成内容，成本低。）

`ProviderResponse` 加 `pub notices: Vec<Notice>`，`Notice { level, text }`（core 定义或 llm 本地——**定义在 core**，见 Task 1 已有 `NoticeLevel`；此处 `ProviderResponse` 用 `Vec<String>` 最简单，或复用 `ProviderEvent::Notice`。**采用**：`ProviderResponse { content, stop_reason, notices: Vec<(NoticeLevel, String)> }`）。

- [ ] **Step 4: Run test to verify it passes**

Run: `cd yi-agent-rs && cargo test -p yi-agent-llm --test openai_responses`
Expected: PASS。

- [ ] **Step 5: Regression check**

Run: `cd yi-agent-rs && cargo test -p yi-agent-llm`
Expected: 全绿（含既有 chat 用例）。

- [ ] **Step 6: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-llm/src crates/yi-agent-llm/tests/openai_responses.rs crates/yi-agent-core/src
git commit -m "feat(llm): auto-probe channel and transparent fallback to chat"
```

---

### Task 6: effort 被拒 → 自动剥离重投

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/openai/error.rs`
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/openai/client.rs`
- Test: `yi-agent-rs/crates/yi-agent-llm/tests/openai_responses.rs`

**Interfaces:**
- Consumes: Task 5 的 `ChannelCache`。
- Produces: `error::is_effort_rejection(status: u16, body: &str) -> bool`；`OpenaiProvider.effort_reject_cache: Arc<Mutex<HashSet<String>>>`（key = `base_url|model|channel`）。

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn unsupported_effort_is_stripped_and_retried_with_a_notice() {
    let server = MockServer::start().await;
    // 第一次：带 reasoning_effort 的 chat 请求 → 400（模拟 Sol 拒 none/minimal）
    Mock::given(method("POST")).and(path("/v1/chat/completions"))
        .and(body_partial_json(serde_json::json!({"reasoning_effort":"minimal"})))
        .respond_with(ResponseTemplate::new(400).set_body_string(
            r#"{"error":{"message":"Unsupported value","param":"reasoning_effort","code":"unsupported_value"}}"#))
        .mount(&server).await;
    // 第二次：不带该字段 → 200
    Mock::given(method("POST")).and(path("/v1/chat/completions"))
        .and(|r: &wiremock::Request| !String::from_utf8_lossy(&r.body).contains("reasoning_effort"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SSE_TEXT_ONLY))
        .mount(&server).await;

    let provider = OpenaiProvider::new(OpenaiProviderOpts {
        base_url: Some(server.uri()), api_key: Some("k".into()),
        timeout: Some(Duration::from_secs(5)),
        api_style: Some("chat".into()), reasoning_effort: Some("minimal".into()),
    }).unwrap();
    let resp = provider.call(simple_request()).await.expect("strip+retry must succeed");
    assert_eq!(resp.stop_reason, StopReason::EndTurn);
    assert!(resp.notices.iter().any(|(_, t)| t.contains("推理档")));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent-llm --test openai_responses unsupported_effort`
Expected: FAIL（400 直接变成错误返回）。

- [ ] **Step 3: Implement**

`error.rs`：

```rust
/// 该 400/422 是否为「模型不支持所配 effort」类拒绝。
/// 实测 horizon 回 `param: "reasoning_effort"` 或 `param: "reasoning.effort"`。
pub fn is_effort_rejection(status: u16, body: &str) -> bool {
    matches!(status, 400 | 422)
        && (body.contains("\"reasoning_effort\"") || body.contains("reasoning.effort"))
        && (body.contains("Supported value") || body.contains("does not support")
            || body.contains("not supported") || body.contains("unsupported"))
}
```

`client.rs`：非成功响应分支加一层：

```rust
if is_effort_rejection(status, &body_text) && !effort_already_stripped {
    self.cache_effort_reject(model_key);
    return self.retry_without_effort(req, "该模型不支持所配推理档，已忽略").await;
}
```

`retry_without_effort`：把请求 params 的 `reasoning_effort` 置 `None` 后按同 channel 重投一次，并把 Notice 塞进最终 `ProviderResponse`。已缓存拒绝的 (base_url,model,channel) 后续请求直接不带 effort。

- [ ] **Step 4: Run test to verify it passes**

Run: `cd yi-agent-rs && cargo test -p yi-agent-llm --test openai_responses`
Expected: PASS。

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-llm/src
git commit -m "feat(llm): strip rejected reasoning effort and retry once"
```

---

### Task 7: app-server（thread meta 覆盖 + RPC + 广播）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs:27-60,396-430`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`thread/setModel` 旁加同级分支，`server.rs:3770` 起）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/model_rpc.rs`（`effective_view` 带 effort）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` 内 `mod tests` 与 `thread_store.rs` 内 `mod tests`

**Interfaces:**
- Consumes: Task 2 的 `ModelEntry.reasoning_effort`。
- Produces:
  - `ThreadMeta.reasoning_effort: Option<String>`
  - `ThreadStore::set_reasoning_effort(id, Option<&str>) -> io::Result<()>`
  - RPC `thread/setReasoningEffort`（params `{threadId, effort: string|null}`）
  - 通知 `thread/reasoningEffortChanged`

- [ ] **Step 1: Write the failing tests**

`thread_store.rs` `mod tests`：

```rust
#[test]
fn reasoning_effort_round_trips_and_legacy_defaults_none() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = ThreadStore::open(dir.path().join("t.json")).unwrap();
    // 旧 meta 无字段 → None
    let id = store.create("cwd", "m", None).unwrap();
    assert_eq!(store.load(&id).unwrap().meta.reasoning_effort, None);
    store.set_reasoning_effort(&id, Some("high")).unwrap();
    assert_eq!(store.load(&id).unwrap().meta.reasoning_effort.as_deref(), Some("high"));
    store.set_reasoning_effort(&id, None).unwrap();
    assert_eq!(store.load(&id).unwrap().meta.reasoning_effort, None);
}
```

`server.rs` `mod tests`（照 `thread_set_model_*` 用例形状）：

```rust
#[tokio::test]
async fn thread_set_reasoning_effort_persists_and_broadcasts() {
    // 起 host（复用既有 setModel 夹具），upsert 一条模型 B，建 thread
    // 发 thread/setReasoningEffort {threadId, effort:"high"}
    // 断言：响应 ok、meta.reasoning_effort == Some("high")、订阅端收到
    //       method == "thread/reasoningEffortChanged" 的通知
}

#[tokio::test]
async fn an_observe_client_cannot_set_reasoning_effort() { /* 期望 -32014 */ }

#[tokio::test]
async fn invalid_effort_value_is_rejected() { /* effort:"bogus" → -32602 */ }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib reasoning_effort`
Expected: 编译/断言失败。

- [ ] **Step 3: Implement thread_store**

`ThreadMeta` 加 `#[serde(default)] pub reasoning_effort: Option<String>`；所有字面量构造点补 `reasoning_effort: None`。加：

```rust
pub fn set_reasoning_effort(&self, id: &str, effort: Option<&str>) -> io::Result<()> {
    self.update_meta(id, |meta| { meta.reasoning_effort = effort.map(str::to_string); })
}
```

（`update_meta` 用既有读-改-写 + 原子写路径，与 `set_model` 同锁。）

- [ ] **Step 4: Implement RPC + 广播**

`protocol.rs` 加方法名常量与通知名常量（照 `thread/modelChanged` 的既有写法）。
`server.rs` 在 `"thread/setModel"` 分支后加 `"thread/setReasoningEffort"`：

- `Control` 档门禁（同 setModel）
- 解析 `effort`：`None|Null` → 清覆盖；`String` 校验在七档白名单内，否则 `-32602`
- 校验通过后：向在场 driver 发 `SessionCommand::SetReasoningEffort { effort, reply }`（新增变体；driver 重建 agent 时把 effort 写进 `AgentConfig.gen_params.reasoning_effort`）
- 落盘 `store.set_reasoning_effort`
- 广播 `thread/reasoningEffortChanged { threadId, effort }`

`thread/resume` / `thread/start` 响应带 `reasoning_effort`；`model/list` 的 `effective_view` 与 `entry_view` 带 `reasoning_effort` / `api_style`。

- [ ] **Step 5: Run test to verify it passes**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib`
Expected: 全绿（既有 480+ 用例 + 新增）。

- [ ] **Step 6: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-app-server/src
git commit -m "feat(app-server): per-thread reasoning effort override"
```

---

### Task 8: TUI（`/effort` + 推理块 + Notice 行）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/slash.rs`（命令枚举 + 解析 + help）
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:2261` 附近（`/effort` 处理）
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/history.rs`（`ReasoningDelta` → 推理块；`Notice` → 状态行）
- Test: `yi-agent-rs/crates/yi-agent/src/tui/app.rs` 内 `mod tests`；`history.rs` 内 `mod tests`

**Interfaces:**
- Consumes: Task 1 的 `ProviderEvent::ReasoningDelta/Notice`；Task 7 的 RPC/meta。
- Produces: `SlashCommand::Effort(Option<String>)`；`ControlCommand::SetReasoningEffort(Option<String>)`；`HistoryCell::Reasoning { text }`（dim 块）。

- [ ] **Step 1: Write the failing tests**

```rust
// slash.rs
#[test]
fn parses_effort_command() {
    assert_eq!(parse("/effort high"), Some(SlashCommand::Effort(Some("high".into()))));
    assert_eq!(parse("/effort"), Some(SlashCommand::Effort(None)));
    assert_eq!(parse("/effort bogus"), None); // 非白名单 → usage/报错
}

// app.rs（照 handle_model_command 的形状）
#[test]
fn effort_command_sets_the_session_override() {
    // /effort high → Some(ControlCommand::SetReasoningEffort(Some("high")))
    // /effort default → Some(ControlCommand::SetReasoningEffort(None))
    // /effort（无参）→ None 且 history 里出现当前生效值与来源
}

// history.rs
#[test]
fn reasoning_delta_accumulates_into_a_reasoning_block() {
    // 连续两条 ReasoningDelta{Content} 合并进同一 HistoryCell::Reasoning
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent effort`
Expected: 编译失败。

- [ ] **Step 3: Implement**

- `slash.rs`：枚举加 `Effort(Option<String>)`，解析 `/effort [档位|default]`，白名单校验；help 文本加一行 `/effort`。
- `app.rs`：`handle_effort_command(arg, current_effort, source, history, width) -> Option<ControlCommand>`，形状照 `handle_model_command`（`2261` 起）。`run_loop` 把 `ControlCommand::SetReasoningEffort` 下发并维护「当前生效值」标记。
- `history.rs`：`AgentEvent` → `HistoryCell` 的转换加 `ReasoningDelta`（累积进 dim `Reasoning` 块）与 `Notice`（状态行 `Separator { label }`）。
- 推理块渲染：dim 前景色前缀「思考」；`/reasoning`（可选，若成本低）折叠开关——若本期不做开关，默认常显。

- [ ] **Step 4: Run test to verify it passes**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent`
Expected: 全绿。

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent/src/tui
git commit -m "feat(tui): /effort command and reasoning block rendering"
```

---

### Task 9: Desktop（设置字段 + 高级区 + 主界面控件）

**Files:**
- Modify: `desktop/src/lib/models.ts`（`ModelEntryView`/`ModelUpsertInput`/`listModels` 带新字段）
- Modify: `desktop/src/lib/modelPicker.ts`（新增 `buildEffortOptions`）
- Modify: `desktop/src/components/SettingsModelsTab.tsx`（effort 字段 + 高级折叠）
- Modify: `desktop/src/components/ModelPicker.tsx`（主界面 effort 控件）
- Test: `desktop/src/lib/modelPicker.test.ts`、`desktop/src/components/SettingsModelsTab.test.tsx`、`desktop/src/components/ModelPicker.test.tsx`

**Interfaces:**
- Consumes: Task 7 的 RPC `thread/setReasoningEffort`。
- Produces: `REASONING_EFFORTS = ["none","minimal","low","medium","high","xhigh","max"]`；`buildEffortOptions(current, entryDefault) -> EffortOption[]`。

- [ ] **Step 1: Write the failing tests**

`modelPicker.test.ts`：

```ts
it("builds effort options with a 未设置 sentinel and marks the current one", () => {
  const opts = buildEffortOptions(null, "low");
  expect(opts[0].value).toBe(null);
  expect(opts.map(o => o.value)).toEqual([null, "none", "minimal", "low", "medium", "high", "xhigh", "max"]);
  expect(opts.find(o => o.value === null)?.label).toContain("low"); // 显示继承自条目默认
});
```

`SettingsModelsTab.test.tsx`：

```tsx
it("sends reasoning_effort and api_style on upsert, kept behind the advanced section", async () => {
  // 打开编辑 → 选 effort=high → 展开「高级」→ 选 responses → 保存
  // 断言 upsertModel 收到 { reasoning_effort: "high", api_style: "responses" }
});
it("advanced section is collapsed by default", () => {
  // 断言 api_style 选择器在未点开前不可见
});
```

`ModelPicker.test.tsx`：

```tsx
it("writes the session effort override and reflects the server-returned value", async () => {
  // 点 effort 控件选 high → 断言 rpc 收到 thread/setReasoningEffort {threadId, effort:"high"}
});
it("shows the entry default when no session override is set", () => { /* 标签含条目默认值 */ });
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd desktop && npx vitest run src/lib/modelPicker.test.ts src/components/SettingsModelsTab.test.tsx src/components/ModelPicker.test.tsx`
Expected: FAIL。

- [ ] **Step 3: Implement lib**

`models.ts`：`ModelEntryView` 加 `api_style?: string | null`、`reasoning_effort?: string | null`；`ModelUpsertInput` 加两个可选字段；`upsertModel` 仅在 `!== undefined` 时塞进 params（与 `api_key` 同一写法）。新增：

```ts
export async function setThreadReasoningEffort(rpc: ModelRpc, threadId: string, effort: string | null): Promise<{ reasoning_effort: string | null }> {
  const r = await rpc<{ reasoning_effort?: unknown }>("thread/setReasoningEffort", { threadId, effort });
  return { reasoning_effort: typeof r?.reasoning_effort === "string" ? r.reasoning_effort : null };
}
```

`modelPicker.ts`：

```ts
export const REASONING_EFFORTS = ["none","minimal","low","medium","high","xhigh","max"] as const;
export type EffortOption = { value: string | null; label: string; selected: boolean };
export function buildEffortOptions(current: string | null, entryDefault: string | null): EffortOption[] {
  const inherit = entryDefault ? `未设置（继承 ${entryDefault}）` : "未设置";
  return [
    { value: null, label: inherit, selected: current === null },
    ...REASONING_EFFORTS.map((e) => ({ value: e as string, label: e, selected: current === e })),
  ];
}
```

- [ ] **Step 4: Implement components**

`SettingsModelsTab.tsx`：`Draft` 加 `reasoning_effort: string | null`、`api_style: string`；编辑表单加「推理档位」正常字段（选项来自 `buildEffortOptions`，含「未设置」）；底部加 `▸ 高级` 折叠（`useState(false)`），展开后显示 `api_style` 三选一；`saveDraft` 把两字段放进 `ModelUpsertInput`。
`ModelPicker.tsx`：在模型下拉旁加一个 effort 小下拉（与模型下拉同风格），值来自 `buildEffortOptions(currentRef 的条目默认, ...)`，选择后调 `setThreadReasoningEffort` 并以回包为准更新；无会话时置灰。

- [ ] **Step 5: Run test to verify it passes**

Run: `cd desktop && npx vitest run && npx tsc --noEmit && npm run build`
Expected: 全绿。

- [ ] **Step 6: Commit**

```bash
git add desktop/src
git commit -m "feat(desktop): reasoning effort field, advanced api_style, effort control"
```

---

## 计划自审记录

**Spec 覆盖对照：**

| Spec 章节 | 覆盖任务 |
|---|---|
| §4.1 条目两字段 | Task 2 |
| §4.2 三级解析链 | Task 2（条目→cfg）+ Task 7（会话覆盖） |
| §5.1 通道分派 | Task 3 + Task 5 |
| §5.2 chat 唯一改动 | Task 3 |
| §5.3 auto 探测 + 回退 | Task 5 |
| §5.4 responses 请求/事件映射 | Task 4 |
| §5.5 effort 剥离 | Task 6 |
| §5.6 core 类型扩展 | Task 1 |
| §6 CoT 展示 | Task 8（TUI）+ Task 9（App）+ Task 1（事件） |
| §7 回退可见性 | Task 5/6（Notice）+ Task 8/9（渲染） |
| §8 三端入口 | Task 7（RPC）+ Task 8（TUI）+ Task 9（App） |
| §9 错误处理 | Task 3/5/6 |
| §10 测试策略 | 各任务 TDD 步骤 |
| §11 分阶段 | Task 1–6 = P1/P2；Task 8/9 = P3/P4 |

**类型一致性检查（已修复一处真实重复）：** `ApiStyle` 初稿在 `config.rs`（Task 2）与 `channel.rs`（Task 5）各定义一份——这是会导致漂移的重复。已收敛：**唯一定义在 `yi-agent-core`（Task 1）**，经 `lib.rs` 导出，Task 2 的 `RuntimeConfig.api_style` 与 Task 5 的 provider 均引用 `yi_agent_core::ApiStyle`；Task 5 的 `channel.rs` 只保留 `Channel`、`ChannelCache`、`looks_like_missing_route`。计划正文已同步（无残留的第二份定义）。

**占位符扫描：** 唯一非完整代码步是 Task 7 的三个 server 集成测试，已明确标注「照 `thread_set_model_*` 既有用例群的形状」并要求执行者先读该用例群（因这些测试依赖庞大的 host 夹具，逐字复制会失真）。其余每个代码步都含可直接落地的完整片段。Task 8 的 `/reasoning` 折叠开关标为可选，符合 YAGNI。
