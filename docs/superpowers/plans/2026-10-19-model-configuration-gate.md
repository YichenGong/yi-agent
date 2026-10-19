# 模型配置门禁实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让模型配置只能由用户显式确认，且必须连接测试通过才允许落盘；运行期不再凭空生成模型名。

**Architecture:** 三层收口。(1) 运行时 `RuntimeConfig` 保留 provider 的默认 URL，但不再提供默认 model；(2) RPC 层 `onboarding/apply` 在写入前自己跑一次连接测试（可注入探测函数，便于测试），并在 `thread/start`、`thread/resume` 上显式拦截空模型；(3) 桌面端向导删除默认值回填与「仍然保存」，改为必填 + 测试通过才能完成。

**Tech Stack:** Rust（`yi-agent-runtime`、`yi-agent-app-server`、`yi-agent` CLI）、React + TypeScript + Vitest（`desktop`）、wiremock（Rust 测试）。

## Global Constraints

- 不得在 `main` 分支上提交；所有改动在 `.worktrees/<branch>` 内完成。
- 提交前必须 `cd yi-agent-rs && cargo fmt --all`。
- 提交信息用 conventional commits，不写 `Co-Authored-By`。
- 测试按 crate 分别跑，不要 `cargo test --workspace`（见 AGENTS.md 的 OOM 说明）。
- 不在 `cargo test` 中调用真实 LLM API；所有连接测试用 wiremock 桩。
- `data.code` 是 UI 分支用的稳定词汇，数值码只是兜底；新增错误必须两者都给。
- 桌面端 1417 个单测必须全绿；`npx tsc --noEmit` 必须无错。

---

### Task 1: 让「空 api_url = provider 默认地址」只有一种解释

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/onboarding.rs:186-227`（`import_settings_into_catalog`）
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/onboarding.rs:272-300`（`test_connection`）
- Test: `yi-agent-rs/crates/yi-agent-runtime/src/onboarding.rs`（内联 `#[cfg(test)]`）

**Interfaces:**
- Consumes: `crate::models::ModelProvider::parse`（现有）。
- Produces:
  - `impl ModelSettings { pub fn with_default_url(&self) -> ModelSettings }`
  - `test_connection` 与 `import_settings_into_catalog` 都先过一遍 `with_default_url`。

**为什么需要这个任务:** 向导允许 `api_url` 留空（语义是"用 provider 默认地址"）。探针 `test_connection` 现在把 `settings.api_url` 原样塞进 `OpenaiProviderOpts { base_url: Some("") }`，`OpenaiProvider::new` 只做 `trim_end_matches('/')`，结果是 `base_url = ""`，请求必然失败。落盘路径 `import_settings_into_catalog` 则自己补了默认地址。两条路径对同一个空串给了两种解释——门禁一落地，"留空 URL + 正确密钥"会被永久判定为连接失败，用户永远存不进去。

- [ ] **Step 1: 写会失败的测试**

在 `onboarding.rs` 的测试模块加：

```rust
#[test]
fn an_empty_url_resolves_to_the_provider_default() {
    let openai = ModelSettings {
        provider: "openai".into(),
        model: "gpt-4o".into(),
        api_url: String::new(),
        api_key: "sk-x".into(),
    };
    assert_eq!(openai.with_default_url().api_url, "https://api.openai.com/v1");

    let anthropic = ModelSettings {
        provider: "anthropic".into(),
        model: "m".into(),
        api_url: "  ".into(),
        api_key: "sk-x".into(),
    };
    assert_eq!(
        anthropic.with_default_url().api_url,
        "https://api.anthropic.com"
    );
}

#[test]
fn an_explicit_url_is_kept_and_trimmed() {
    let settings = ModelSettings {
        provider: "openai".into(),
        model: "m".into(),
        api_url: "  https://gw.example/v1  ".into(),
        api_key: "k".into(),
    };
    assert_eq!(settings.with_default_url().api_url, "https://gw.example/v1");
}
```

- [ ] **Step 2: 运行测试，确认它失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime --lib with_default_url`
Expected: 编译失败，报 `ModelSettings` 没有 `with_default_url`。

- [ ] **Step 3: 实现并让两条路径共用**

在 `ModelSettings` 的 `impl` 块（若没有就在结构体定义之后新建）加：

```rust
impl ModelSettings {
    /// 把「留空 = 用 provider 默认地址」这条规则收在一处。
    ///
    /// 探测与落盘必须看到同一份解析结果：否则向导留空时，探测打到空
    /// `base_url`（`OpenaiProvider::new` 只做 `trim_end_matches('/')`），
    /// 而落盘却存了默认地址，门禁会把合法配置永久判成失败。
    ///
    /// 非法 provider 这里不报错（返回空串），由 `validate_settings` 负责拒绝。
    pub fn with_default_url(&self) -> ModelSettings {
        let default_url = match crate::models::ModelProvider::parse(&self.provider) {
            Some(crate::models::ModelProvider::Openai) => "https://api.openai.com/v1",
            Some(crate::models::ModelProvider::Anthropic) => "https://api.anthropic.com",
            None => "",
        };
        let api_url = if self.api_url.trim().is_empty() {
            default_url.to_string()
        } else {
            self.api_url.trim().to_string()
        };
        ModelSettings {
            provider: self.provider.clone(),
            model: self.model.trim().to_string(),
            api_url,
            api_key: self.api_key.clone(),
        }
    }
}
```

把 `import_settings_into_catalog` 开头的四处默认地址计算删掉，改为：

```rust
pub fn import_settings_into_catalog(
    models_path: &Path,
    settings: &ModelSettings,
) -> Result<String, String> {
    let settings = settings.with_default_url();
    let Some(provider) = crate::models::ModelProvider::parse(&settings.provider) else {
        return Err(format!("unknown provider: {}", settings.provider));
    };
    let mut catalog = crate::models::load_catalog_from(models_path);
    let base = settings.model.clone();
    let mut name = base.clone();
    let mut n = 2u32;
    while catalog.models.iter().any(|m| m.name == name) {
        name = format!("{base}-{n}");
        n += 1;
    }
    catalog.models.push(crate::models::ModelEntry {
        name: name.clone(),
        provider,
        api_url: settings.api_url.clone(),
        model: settings.model.clone(),
        api_key: settings.api_key.clone(),
        api_style: None,
        reasoning_effort: None,
        reasoning_capabilities: None,
        context_length: None,
    });
    catalog.default_model = Some(name.clone());
    crate::models::save_catalog_to(models_path, &catalog).map_err(|e| e.to_string())?;
    Ok(name)
}
```

在 `test_connection` 的第一行加：

```rust
pub async fn test_connection(settings: &ModelSettings) -> ConnectionOutcome {
    // 与落盘同口径：空 url 在这里补成 provider 默认地址。
    let settings = &settings.with_default_url();
    use yi_agent_core::{GenParams, Message, Provider, ProviderRequest};
    // ...以下与现有一致（settings 现在是本地借用，字段访问不变）
```

- [ ] **Step 4: 运行测试，确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime --lib onboarding`
Expected: PASS（既有 `import_settings_into_catalog` 的断言应保持全绿）。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-runtime/src/onboarding.rs
git commit -m "fix(onboarding): 探测与落盘统一解析空 api_url"
```

---

### Task 2: 在 RPC 边界加异步门禁，并新增错误族

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs:205-215`（新增 `model_test_failed`、`model_not_configured`）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/onboarding.rs:40-70`
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs`

**Interfaces:**
- Consumes: Task 1 的 `ModelSettings::with_default_url`；`yi_agent_runtime::onboarding::{settings_from_params, test_connection}`（现有）。
- Produces:
  - `RpcError::model_test_failed(reason: impl Into<String>) -> Self`（`code = -32602`，`data.code = "model_test_failed"`）
  - `RpcError::model_not_configured(reasons: &[String]) -> Self`（`code = -32027`，`data.code = "model_not_configured"`，`data.reasons`）

**关键设计约束:** 门禁**只**放在 `rpc/onboarding.rs` 的 async 分支。

`onboarding_rpc::handle_onboarding_request_at` 是同步函数，它的 `apply` 只做校验 + 写盘，**不发网络请求**。原因有三：

1. `futures::executor::block_on` 在 tokio worker 线程上会 panic（"cannot block the current thread from within a runtime"），而 RPC 分支运行在 tokio 运行时里；
2. 若同步写盘函数自己探测，async 分支又探一次，生产路径会发两次完全相同的请求；
3. 写盘入口是进程内调用，客户端只能通过 RPC 到达它，门禁放在 RPC 边界即覆盖所有客户端。

因此在 `apply` 上加一行注释固化这条分层，防止后人把探测又挪回同步函数。

- [ ] **Step 1: 写会失败的测试**

在 `rpc_tests.rs` 加：

```rust
/// `onboarding/apply` 在连接测试失败时必须零写入并给出稳定错误码。
#[tokio::test(flavor = "multi_thread")]
async fn onboarding_apply_refuses_when_the_probe_fails() {
    let mut h = Harness::new();
    initialize(&mut h).await;
    // 端口 1 上没有监听者：连接被立即拒绝，不会等满 15s 超时。
    h.send(
        r#"{"jsonrpc":"2.0","id":2,"method":"onboarding/apply","params":{"provider":"anthropic","model":"m","api_url":"http://127.0.0.1:1","api_key":"k"}}"#,
    )
    .await;
    let v = h.read_value().await;
    assert_eq!(v["error"]["data"]["code"], "model_test_failed", "got {v}");
    assert_eq!(v["error"]["code"], -32602, "got {v}");

    // 清单没有被写进去，状态仍是 needed。
    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"onboarding/status","params":{}}"#)
        .await;
    let v = h.read_value().await;
    assert_eq!(v["result"]["needed"], true, "got {v}");
}
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib onboarding_apply_refuses_when_the_probe_fails`
Expected: FAIL（当前 apply 直接成功，没有 `model_test_failed`）。

- [ ] **Step 3: 实现错误族**

在 `protocol.rs` 的 `impl RpcError` 内（`invalid_model` 之后）加：

```rust
/// 保存模型前的一次连接测试失败。
///
/// 与 `invalid_model` 共用 `-32602` 族（都属于"调用方给的配置不能用"），
/// 但 `data.code` 是 UI 分支用的稳定词汇：它要把"连通性失败"和"字段非法"
/// 说成两件事，否则用户会被引导去改字段而不是查网络/密钥。
pub fn model_test_failed(reason: impl Into<String>) -> Self {
    Self {
        code: -32602,
        message: format!("connection test failed: {}", reason.into()),
        data: Some(serde_json::json!({ "code": "model_test_failed" })),
    }
}

/// 整台机器还没有可用模型（清单无默认条目，也没有显式注入）。
///
/// 数值码自成一族：这不是参数错误，UI 应把用户导向设置/引导，而不是让他
/// 去改当前这次调用的参数。
pub fn model_not_configured(reasons: &[String]) -> Self {
    Self {
        code: -32027,
        message: "no model is configured; finish model setup first".to_string(),
        data: Some(serde_json::json!({
            "code": "model_not_configured",
            "reasons": reasons,
        })),
    }
}
```

- [ ] **Step 4: 把门禁接进 async 分支**

把 `rpc/onboarding.rs` 里合并的 `"onboarding/apply" | "onboarding/dismiss"` 臂拆成两个。`apply` 臂：

```rust
"onboarding/apply" => {
    if state.client_scope < Scope::Control {
        write_response(
            state.hub,
            state.client,
            err_response(id, RpcError::insufficient_scope(Scope::Control)),
        )
        .await?;
        return Ok(Flow::Continue);
    }
    let settings = match crate::onboarding_rpc::settings_from_params(&req.params) {
        Ok(s) => s,
        Err(error) => {
            write_response(state.hub, state.client, err_response(id, error)).await?;
            return Ok(Flow::Continue);
        }
    };
    // 门禁在服务端：任何客户端都不能绕过"测试通过"。
    // 空 api_url 在这里补成 provider 默认地址——与落盘同一个解析函数，
    // 否则向导留空唯一合法的配置会被判成失败。
    let resolved = settings.with_default_url();
    let outcome = yi_agent_runtime::onboarding::test_connection(&resolved).await;
    if !outcome.ok {
        write_response(
            state.hub,
            state.client,
            err_response(
                id,
                RpcError::model_test_failed(
                    outcome.reason.unwrap_or_else(|| "连接测试未通过".into()),
                ),
            ),
        )
        .await?;
        return Ok(Flow::Continue);
    }
    // 探测已通过，这里只剩校验 + 写盘：同步入口不再发第二次网络请求。
    match crate::onboarding_rpc::handle_onboarding_request_at(
        state.onboarding_env_path,
        state.onboarding_preferences_path,
        state.models_path,
        "onboarding/apply",
        &req.params,
        state.cfg,
    ) {
        Ok(value) => write_response(state.hub, state.client, ok_response(id, value)).await?,
        Err(error) => {
            write_response(state.hub, state.client, err_response(id, error)).await?
        }
    }
}
"onboarding/dismiss" => {
    if state.client_scope < Scope::Control {
        write_response(
            state.hub,
            state.client,
            err_response(id, RpcError::insufficient_scope(Scope::Control)),
        )
        .await?;
        return Ok(Flow::Continue);
    }
    match crate::onboarding_rpc::handle_onboarding_request_at(
        state.onboarding_env_path,
        state.onboarding_preferences_path,
        state.models_path,
        "onboarding/dismiss",
        &req.params,
        state.cfg,
    ) {
        Ok(value) => write_response(state.hub, state.client, ok_response(id, value)).await?,
        Err(error) => {
            write_response(state.hub, state.client, err_response(id, error)).await?
        }
    }
}
```

在 `onboarding_rpc::apply` 上加注释，固化分层：

```rust
fn apply(
    _env_path: &Path,
    models_path: &Path,
    params: &Value,
    _fallback: &RuntimeConfig,
) -> Result<Value, RpcError> {
    // 注意：这里**只做校验 + 写盘**，不做连接探测。
    // 探测是 async 的，必须由 `rpc/onboarding.rs` 的 async 分支负责：
    // 同步函数里 `block_on` 会在 tokio worker 上 panic，而且会和 RPC 层
    // 各探一次、白发一轮请求。客户端只能经 RPC 到达这里，门禁不会漏。
    let settings = settings_from_params(params)?;
    ...
}
```

- [ ] **Step 5: 修正既有测试**

`onboarding_apply_lands_the_env_and_catalog` 现在会被门禁拦下（`api_url` 为空 → 打到真实默认地址）。把它改名为 `onboarding_apply_lands_the_catalog_after_a_passing_probe`，并把 `onboarding_test_probes_the_endpoint` 里的 anthropic SSE 桩抽成模块级辅助函数 `anthropic_sse_server() -> MockServer`（两处复用，桩内容逐字保持不变：`POST /v1/messages` 返回 `text/event-stream`）。

```rust
#[tokio::test(flavor = "multi_thread")]
async fn onboarding_apply_lands_the_catalog_after_a_passing_probe() {
    let server = anthropic_sse_server().await;
    let mut h = Harness::new();
    initialize(&mut h).await;
    let req = serde_json::json!({
        "jsonrpc":"2.0","id":2,"method":"onboarding/apply",
        "params":{"provider":"anthropic","model":"m","api_url":server.uri(),"api_key":"sk-x"}
    });
    h.send(&req.to_string()).await;
    let v = h.read_value().await;
    assert_eq!(v["result"]["ok"], true, "got {v}");
    assert!(v["result"].get("name").is_some(), "got {v}");
    assert!(v["result"].get("env_written").is_none(), "got {v}");
    assert!(v["result"].get("imported").is_none(), "got {v}");

    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"onboarding/status","params":{}}"#)
        .await;
    let v = h.read_value().await;
    assert_eq!(v["result"]["needed"], false, "got {v}");
}
```

`an_observe_client_cannot_apply_onboarding` 不受影响（scope 检查在探测之前）。

`onboarding_rpc.rs` 里的同步单测（`status_reports_ready_once_the_catalog_is_complete`、`apply_writes_the_catalog_and_never_the_env` 等）直接调 `handle_onboarding_request_at`，绕过了 RPC 门禁，因此**不需要**改：它们测的是写盘语义，且不触网。这也是有意的分层。

- [ ] **Step 6: 运行测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib onboarding`
Expected: PASS。

- [ ] **Step 7: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-app-server/src/protocol.rs crates/yi-agent-app-server/src/rpc/onboarding.rs crates/yi-agent-app-server/src/onboarding_rpc.rs crates/yi-agent-app-server/src/rpc_tests.rs
git commit -m "feat(onboarding): apply 必须连接测试通过才落盘"
```

---


### Task 3: 迁移不再臆造模型名

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/migrate.rs:8-30`（新增一个 `MigrationOutcome` 变体）
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/migrate.rs:176-190`（去掉默认 model）
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/migrate.rs:228-257`（`log_outcome`）
- Test: `yi-agent-rs/crates/yi-agent-runtime/src/migrate.rs`（内联）

**为什么需要这个任务:** `migrate_model_keys` 在旧 `.env` 有 `YI_AGENT_PROVIDER` 与 `MODEL_API_KEY`、但**没有** `YI_AGENT_MODEL` 时，会把 `gpt-4o` / `claude-sonnet-4-20250514` 写进清单并设为默认（`migrate.rs:182-183`）。它跑在侧车与 CLI 的启动路径上（`server.rs:1944`、`main.rs:56,1185`），完全绕过了本设计的门禁——用户什么都没确认，清单里就多了一条"已配置"的条目，子 agent 照旧拿到臆造名。修掉 `config.rs` 的默认值却不修这里，问题会原样复活。

**取舍:** 不能把空 model 写进清单——`parse_catalog` 会丢弃 `model` 为空的条目（`models.rs:464-476`），若整份 `models` 数组只写一条则直接报错（`models.rs:485-489`）。也不能悄悄跳过并注释 `.env`，那会把用户的密钥注释掉却不给出任何可配置的条目。正确做法是：**不落清单、不动 `.env`**，返回一个新的可识别结果，让日志与后续引导告诉用户"还差模型标识"。

- [ ] **Step 1: 写会失败的测试**

在 `migrate.rs` 的测试模块加：

```rust
#[test]
fn a_model_less_env_is_not_turned_into_an_invented_entry() {
    let dir = tempfile::TempDir::new().unwrap();
    let env = write_env(dir.path(), "YI_AGENT_PROVIDER=openai\nMODEL_API_KEY=sk-x\n");
    let models = dir.path().join("models.json");
    assert_eq!(
        migrate_model_keys(&env, &models),
        MigrationOutcome::MissingModel
    );
    assert!(!models.exists(), "nothing may be written without a model");
    let text = std::fs::read_to_string(&env).unwrap();
    assert!(
        text.contains("MODEL_API_KEY=sk-x"),
        "the key must stay put so the user can finish setup: {text}"
    );
}
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime --lib a_model_less_env_is_not_turned_into_an_invented_entry`
Expected: FAIL（当前返回 `Migrated { entry: "gpt-4o" }`，且清单已写入）。

- [ ] **Step 3: 实现**

在 `MigrationOutcome` 加变体：

```rust
/// 旧 `.env` 里有 provider 与 key，但没有模型标识：无从得知用户想用哪个
/// 模型。不落清单、不改 `.env`，留给引导去问。
MissingModel,
```

在 `migrate_model_keys` 里，`let settings = ...` 之后、构建清单条目之前插入：

```rust
// 没有模型标识就没有可落盘的条目：`parse_catalog` 会丢弃 model 为空的
// 条目，硬写只会得到"清单看似写了、读回却是空的"。也绝不猜一个名字——
// 那正是本次要修的问题。
if settings.model.trim().is_empty() {
    return MigrationOutcome::MissingModel;
}
```

删掉 `let model = if settings.model.trim().is_empty() { ... } else { settings.model.clone() };` 这段兜底，直接用 `settings.model.trim().to_string()`（或保留变量但去掉 match 分支，按编译器提示收敛）。

在 `log_outcome` 加：

```rust
MigrationOutcome::MissingModel => tracing::warn!(
    path = %env_path.display(),
    "legacy model keys in .env have no YI_AGENT_MODEL; .env was left untouched — \
     finish model setup to create a catalog entry"
),
```

- [ ] **Step 4: 运行测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime --lib migrate`
Expected: PASS（既有 4 个迁移测试应保持全绿）。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-runtime/src/migrate.rs
git commit -m "fix(migrate): 缺少模型标识时不再臆造默认名"
```

---

### Task 4: 运行期移除默认 model

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/config.rs:238-300`
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/config.rs:807-845`（测试）
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/config.rs:1000-1015`（测试）
- Test: 同上文件内联测试

**Interfaces:**
- Consumes: 无。
- Produces: `RuntimeConfig::model` 在无注入且清单为空时为空串 `""`；`api_url` 仍有 provider 默认值。

- [ ] **Step 1: 改写测试为期望空 model**

把 `load_defaults_api_url_and_model` 改名为 `load_defaults_the_url_but_not_a_model`：

```rust
#[test]
fn load_defaults_the_url_but_not_a_model() {
    let _lock = ENV_TEST_MUTEX
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let _env = isolated_config_env();
    let overrides = ConfigOverrides {
        provider: Some("anthropic".into()),
        api_url: None,
        api_key: Some("test-key".into()),
        model: None,
        max_turns: None,
        workdir: Some(PathBuf::from(".")),
        system_prompt: None,
        model_context_length: None,
        compact_ratio: None,
        compact_keep_turns: None,
        compact_user_budget_tokens: None,
        compact_tool_budget_tokens: None,
        yolo: false,
        sandbox: None,
        sandbox_writable_roots: Vec::new(),
        skip_permissions: false,
        skills_catalog_budget: None,
    };
    let config = RuntimeConfig::load(&overrides).unwrap();
    assert_eq!(config.api_url, "https://api.anthropic.com");
    // 地址有客观默认值，模型名没有：没有显式注入就必须是空串，由调用方
    // 自己判断"未配置"并报可识别的错误，而不是凭空猜一个名字发出去。
    assert_eq!(config.model, "");
    assert_eq!(config.max_turns, 200);
    assert!(config.system_prompt.is_none());
}
```

把 openai 那段（`:1000-1015`）同样改成：

```rust
assert_eq!(config.provider, "openai");
assert_eq!(config.api_url, "https://api.openai.com/v1");
assert_eq!(config.model, "", "no default model name may be invented");
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime --lib load_defaults_the_url_but_not_a_model`
Expected: FAIL，`config.model` 仍是 `claude-sonnet-4-20250514`。

- [ ] **Step 3: 移除默认 model**

在 `config.rs` 的 `load_inner` 里，把：

```rust
let default_model = match provider.as_str() {
    "openai" => "gpt-4o",
    _ => "claude-sonnet-4-20250514",
};
```

删掉，并把：

```rust
let model = injected_model.unwrap_or_else(|| default_model.to_string());
```

改为：

```rust
// 模型名没有客观默认值：猜一个名字发出去只会得到 404，而且这个猜测会一路
// 传播到子 agent（`subagent_model` → `default_model` → `cfg`）。空串是
// "未配置"的唯一诚实表示，由 RPC 入口与 CLI 给出可操作的指引。
let model = injected_model.unwrap_or_default();
```

- [ ] **Step 4: 运行测试，确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime --lib config`
Expected: PASS（若有其它断言默认 model 的测试，一并按同样口径改为空串）。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-runtime/src/config.rs
git commit -m "fix(runtime): 不再凭空生成默认模型名"
```

---

### Task 5: build_provider 拒绝空模型

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/bootstrap.rs:11-35`
- Test: `yi-agent-rs/crates/yi-agent-runtime/src/bootstrap.rs`（内联测试）

**Interfaces:**
- Consumes: Task 4 的空 model 语义。
- Produces: `build_provider` 在 `cfg.model.trim().is_empty()` 时返回 `Err`，消息为 `"模型未配置：请在设置中完成模型配置"`。

- [ ] **Step 1: 写会失败的测试**

在 `bootstrap.rs` 的测试模块里加：

```rust
#[test]
fn build_provider_rejects_an_empty_model() {
    let mut cfg = sample_config();
    cfg.model = String::new();
    let err = match build_provider(&cfg) {
        Ok(_) => panic!("an empty model must not build a provider"),
        Err(error) => error.to_string(),
    };
    assert!(err.contains("模型未配置"), "got {err}");
}
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime --lib build_provider_rejects_an_empty_model`
Expected: FAIL（当前会成功构造）。

- [ ] **Step 3: 加校验**

在 `build_provider` 函数体第一行加：

```rust
if cfg.model.trim().is_empty() {
    anyhow::bail!("模型未配置：请在设置中完成模型配置");
}
```

- [ ] **Step 4: 运行测试，确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime --lib build_provider`
Expected: PASS（包括既有的 `build_provider_accepts_anthropic_and_openai`）。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-runtime/src/bootstrap.rs
git commit -m "fix(runtime): build_provider 拒绝空模型"
```

---

### Task 6: thread/start 与 thread/resume 显式拦截未配置模型

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/thread.rs:210-280`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/thread.rs:490-520`
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs`

**Interfaces:**
- Consumes: Task 2 的 `RpcError::model_not_configured`；`yi_agent_runtime::onboarding::assess`。
- Produces: 无模型时 `thread/start`、`thread/resume` 返回 `-32027` / `model_not_configured`。

- [ ] **Step 1: 写会失败的测试**

```rust
/// 清单为空且无显式注入时，新建会话必须给出可识别的"未配置模型"错误，
/// 而不是拿一个猜测的名字去发请求。
///
/// **必须显式把 `cfg.model` 置空**：`test_config()` 的 model 是
/// `TEST_MODEL`（`test_support.rs:10,57`，非空），默认 Harness 永远不会
/// 触发拦截——那样这个测试会"通过"却什么都没验证。
#[tokio::test(flavor = "multi_thread")]
async fn thread_start_reports_a_missing_model_configuration() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    cfg.model = String::new();
    let mut h = Harness::with_cfg(cfg).await;
    initialize(&mut h).await;
    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
        .await;
    let v = h.read_value().await;
    assert_eq!(v["error"]["code"], -32027, "got {v}");
    assert_eq!(v["error"]["data"]["code"], "model_not_configured", "got {v}");
}
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_start_reports_a_missing_model_configuration`
Expected: FAIL（当前返回 `-32603` 或成功）。

- [ ] **Step 3: 加校验**

在 `rpc/thread.rs` 的 `"thread/start"` 分支里，`let extra_roots = ...;` 之后、`let built = match ...` 之前插入：

```rust
// 模型可用性在**建 agent 之前**判：建 agent 会做大量装配，失败信息会被
// 压成 internal；这里直接给 UI 一个可分支的错误码。
//
// 判据是「解析后的生效 model 是否为空」，而不是「有没有清单条目」：
// `effective_entry_for` 在整包注入时返回 `None`（表示注入压过清单），
// 那条路径的 model 来自 `cfg.model`，非空就是合法配置。Task 4 已保证
// 「无条目且无注入」时 `cfg.model == ""`，所以一个判据覆盖三种情况：
//   有条目 → 条目.model；无条目+注入 → cfg.model；两者都无 → 空串。
let catalog = yi_agent_runtime::models::load_catalog_from(state.models_path);
let effective_model =
    yi_agent_runtime::models::resolve_effective(state.cfg, &catalog, model_ref.as_deref()).model;
if effective_model.trim().is_empty() {
    let env = yi_agent_runtime::onboarding::read_env_fields(state.onboarding_env_path)
        .unwrap_or_default();
    let reasons = match yi_agent_runtime::onboarding::assess(&env, Some(&catalog)) {
        yi_agent_runtime::onboarding::Assessment::Needed { reasons } => reasons
            .into_iter()
            .map(|m| crate::onboarding_rpc::missing_name(m).to_string())
            .collect(),
        yi_agent_runtime::onboarding::Assessment::Ready { .. } => Vec::new(),
    };
    write_response(
        state.hub,
        state.client,
        err_response(id.clone(), RpcError::model_not_configured(&reasons)),
    )
    .await?;
    return Ok(Flow::Continue);
}
```

`missing_name` 需要从私有改为 `pub(crate)`（它在 `onboarding_rpc.rs:49`）。

`"thread/resume"` 分支同理：在 `let built = match with_session_model_ref(...)` 之前插入同一段校验，把 `model_ref` 换成 `loaded.meta.model_ref.as_deref()`。注意此时 `loaded` 已就绪（`thread.rs:440`），且必须放在 `let built` 之前、`loaded` 之后。

- [ ] **Step 4: 运行测试，确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_start_reports_a_missing_model_configuration`
Expected: PASS

- [ ] **Step 5: 补一个「显式注入不受影响」的测试**

```rust
/// 整包注入（--model + --api-key）时即使清单为空也必须能开会话：
/// 这条路径不经过清单条目，拦截只能看 `cfg.model` 是否为空。
#[tokio::test(flavor = "multi_thread")]
async fn thread_start_accepts_an_explicit_injection_without_a_catalog() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    // Harness 本来就注入隔离的 models_path（空清单）；这里只补注入标记。
    cfg.model = "claude-injected".to_string();
    cfg.api_key = "sk-injected".to_string();
    cfg.model_injected = true;
    let mut h = Harness::with_cfg(cfg).await;
    initialize(&mut h).await;
    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
        .await;
    let v = h.read_value().await;
    assert!(v.get("error").is_none(), "got {v}");
}
```

（`Harness::with_cfg` 是既有构造器，`rpc_tests.rs:1822`；没有 `new_with_injected_model`，不要凭空调用。）

- [ ] **Step 6: 跑既有 thread 测试确认无回归**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread`
Expected: PASS

- [ ] **Step 7: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-app-server/src/rpc/thread.rs crates/yi-agent-app-server/src/onboarding_rpc.rs crates/yi-agent-app-server/src/rpc_tests.rs
git commit -m "feat(app-server): 未配置模型时给出稳定错误码"
```

---

### Task 7: CLI 引导与指引对齐

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/onboarding_prompt.rs:57-115`
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs:1543-1553`
- Test: `yi-agent-rs/crates/yi-agent/src/onboarding_prompt.rs`（若有测试）/ `main.rs`

**Interfaces:**
- Consumes: `test_connection`。
- Produces: CLI 不再回填默认 model；测试不可跳过；`unconfigured_guidance` 在 model 为空时也返回指引。

- [ ] **Step 1: 写会失败的测试**

在 `onboarding_prompt.rs` 的测试模块加（若不易测交互，则改为对纯函数的测试）：

```rust
#[test]
fn an_empty_model_is_rejected_rather_than_defaulted() {
    // `validate_settings` 必须拒绝空 model；默认值回填已经被删掉。
    let settings = yi_agent_runtime::onboarding::ModelSettings {
        provider: "anthropic".into(),
        model: "".into(),
        api_url: "https://api.anthropic.com".into(),
        api_key: "sk-x".into(),
    };
    assert!(yi_agent_runtime::onboarding::validate_settings(&settings).is_err());
}
```

- [ ] **Step 2: 改 CLI 交互**

把：

```rust
let (default_url, default_model) = provider_defaults(&provider);
...
let model = read_line_or(
    &format!("模型标识（默认 {default_model}）: "),
    default_model,
)?;
```

改为（保留 `provider_defaults` 的签名与既有测试 `provider_defaults_cover_both_providers`，只是不再把 model 当默认值用；变量名改成 `_default_model` 或直接用 `provider_defaults(&provider).0`）：

```rust
// 只取 URL 默认值。model 仍由 `provider_defaults` 提供（那个函数与既有测试
// 保持不动），但**不再**作为输入默认值——它是一个没被确认过的猜测。
let (default_url, _default_model) = provider_defaults(&provider);
...
let model = loop {
    let value = read_line("模型标识: ")?;
    if value.is_empty() {
        println!("模型标识不能为空（例如 claude-sonnet-4-20250514 或 gpt-4o）。");
        continue;
    }
    break value;
};
```

把「是否测试连接 / 仍然保存」整段替换为不可跳过的测试：

```rust
println!("正在测试连接…");
let runtime = tokio::runtime::Runtime::new()?;
let outcome = runtime.block_on(onboarding::test_connection(&settings));
if !outcome.ok {
    let reason = outcome.reason.unwrap_or_else(|| "未知原因".into());
    println!("连接失败：{reason}");
    println!("没有保存任何配置。请检查密钥、地址与模型标识后重新运行。");
    // 注意：**不**写 `save_dismissed`。写了它，下次启动 `should_offer` 直接返回
    // false，用户再也见不到引导，只能自己找到设置页——这正是要避免的死角。
    return Ok(());
}
println!("连接正常。");
```

注意 `settings` 的 `api_url` 仍是 `read_line_or(..., default_url)` 的结果（URL 有客观默认值，保留回填合理）；`test_connection` 内部会再归一化一次空串，两者不冲突。

- [ ] **Step 3: 扩展 unconfigured_guidance 的两个调用点**

`unconfigured_guidance` 本身**不改签名**（既有测试 `main.rs:2593-2600` 依赖 `bool`），只改两个调用点的判据：

`main.rs:1213`（`run_agent`）与 `main.rs:1566`（`run_headless`）都改为：

```rust
let configured = !config.api_key.trim().is_empty() && !config.model.trim().is_empty();
if let Some(line) = unconfigured_guidance(configured) {
    eprintln!("{line}");
    std::process::exit(1);
}
```

函数体内的文案补上"模型标识"：

```rust
pub(crate) fn unconfigured_guidance(configured: bool) -> Option<String> {
    if configured {
        None
    } else {
        Some(
            "尚未配置模型：请运行 `yi-agent` 完成初始化，或在设置中配置模型条目\
             （也可用 `--provider`/`--model`/`--api-key` 显式指定）。"
                .to_string(),
        )
    }
}
```

- [ ] **Step 4: 运行测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent --lib onboarding` 与 `cargo test -p yi-agent --lib unconfigured`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent/src/onboarding_prompt.rs crates/yi-agent/src/main.rs
git commit -m "feat(cli): 引导必填模型且连接测试不可跳过"
```

---

### Task 8: 前端 /v1 判定抽成共享纯函数

**Files:**
- Modify: `desktop/src/lib/models.ts`（新增 `looksMissingV1`）
- Modify: `desktop/src/components/SettingsModelsTab.tsx:65-70,425-445`
- Test: `desktop/src/lib/models.test.ts`

**Interfaces:**
- Consumes: `ModelEntryView`。
- Produces: `export function looksMissingV1(provider: string, apiUrl: string): boolean`

- [ ] **Step 1: 写会失败的测试**

在 `desktop/src/lib/models.test.ts` 加：

```ts
import { looksMissingV1 } from "./models";

describe("looksMissingV1", () => {
  it("flags an openai-compatible url without a /v1 suffix", () => {
    expect(looksMissingV1("openai", "https://api.example.com")).toBe(true);
    expect(looksMissingV1("openai", "https://api.example.com/openai")).toBe(true);
  });

  it("accepts a url that ends with /v1, with or without a trailing slash", () => {
    expect(looksMissingV1("openai", "https://api.example.com/v1")).toBe(false);
    expect(looksMissingV1("openai", "https://api.example.com/v1/")).toBe(false);
    expect(looksMissingV1("openai", "  https://api.example.com/v1  ")).toBe(false);
  });

  // 空白 url 的语义是「用 provider 默认地址」（`import_settings_into_catalog`
  // 会补 `https://api.openai.com/v1`）。对它报警告是纯噪音：用户什么都没写错。
  it("stays silent on a blank url, which means the provider default", () => {
    expect(looksMissingV1("openai", "")).toBe(false);
    expect(looksMissingV1("openai", "   ")).toBe(false);
  });

  it("never flags a non-openai provider", () => {
    expect(looksMissingV1("anthropic", "https://api.anthropic.com")).toBe(false);
  });
});
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `cd desktop && npx vitest run src/lib/models.test.ts`
Expected: FAIL（`looksMissingV1` 未导出）。

- [ ] **Step 3: 实现**

在 `desktop/src/lib/models.ts` 加：

```ts
/**
 * OpenAI 兼容接口的地址通常以 `/v1` 结尾。
 *
 * 只做启发式提醒，不下硬结论：`/openai/v1` 这类带前缀的地址同样满足，
 * 因此判定用「去尾部空白与斜杠后是否以 `/v1` 结尾」，而不是「路径里含 /v1」。
 * 写错的后果是 404，而宿主会把它翻成「模型标识不被该地址接受」——这条
 * 提示会误导用户去改模型名，所以必须在保存前提醒。
 *
 * 空白 url **不**提示：它的语义是"用 provider 默认地址"（落盘时补
 * `https://api.openai.com/v1`），报警告只会制造噪音。
 */
export function looksMissingV1(provider: string, apiUrl: string): boolean {
  if (provider !== "openai") return false;
  const trimmed = apiUrl.trim();
  if (trimmed === "") return false;
  return !trimmed.replace(/\/+$/, "").endsWith("/v1");
}
```

把 `SettingsModelsTab.tsx` 的 `suspectsMissingV1` 改为：

```ts
function suspectsMissingV1(entry: ModelEntryView): boolean {
  // 清单条目要求 api_url 非空（`model/upsert` 拒绝空串），所以这里的空白
  // 分支在生产中不可达；保留它只是让共享函数对两种入口都安全。
  return looksMissingV1(entry.provider, entry.api_url);
}
```

并从 `../lib/models` 导入 `looksMissingV1`。

- [ ] **Step 4: 运行测试**

Run: `cd desktop && npx vitest run src/lib/models.test.ts src/components/SettingsModelsTab.test.tsx`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
cd desktop && npx tsc --noEmit
git add src/lib/models.ts src/lib/models.test.ts src/components/SettingsModelsTab.tsx
git commit -m "refactor(desktop): /v1 启发式抽成共享纯函数"
```

---

### Task 9: 向导必填 + 门禁 + /v1 提醒

**Files:**
- Modify: `desktop/src/components/OnboardingWizard.tsx:22-235`
- Modify: `desktop/src/components/OnboardingWizard.test.tsx`

**Interfaces:**
- Consumes: Task 8 的 `looksMissingV1`；现有 `testOnboarding`、`applyOnboarding`。
- Produces: 无（UI 行为）。

- [ ] **Step 1: 改写测试**

把 `offers 'save anyway' after a failed connection test` 替换为：

```tsx
it("keeps save disabled after a failed connection test", async () => {
  const call = vi.fn(async (method: string) => {
    if (method === "onboarding/test") return { ok: false, reason: "无法连接 API 地址" };
    return { ok: true, name: "gpt-4o" };
  });
  const { onDone } = renderWizard(call);
  fireEvent.click(screen.getByRole("button", { name: /开始/ }));
  fireEvent.click(screen.getByRole("button", { name: "openai" }));
  fireEvent.click(screen.getByRole("button", { name: /下一步/ }));
  fireEvent.change(await screen.findByLabelText("API 密钥"), { target: { value: "sk-x" } });
  fireEvent.change(screen.getByLabelText("模型标识"), { target: { value: "gpt-4o" } });
  fireEvent.click(screen.getByRole("button", { name: /测试连接/ }));
  expect(await screen.findByText("无法连接 API 地址")).toBeTruthy();
  const save = screen.getByRole("button", { name: /保存/ }) as HTMLButtonElement;
  expect(save.disabled).toBe(true);
  fireEvent.click(save);
  expect(onDone).not.toHaveBeenCalled();
});

it("disables save until the model field is filled", async () => {
  const call = vi.fn(async () => ({ ok: true }));
  renderWizard(call);
  fireEvent.click(screen.getByRole("button", { name: /开始/ }));
  fireEvent.click(screen.getByRole("button", { name: /下一步/ }));
  fireEvent.change(await screen.findByLabelText("API 密钥"), { target: { value: "sk-x" } });
  const save = screen.getByRole("button", { name: /保存/ }) as HTMLButtonElement;
  expect(save.disabled).toBe(true);
  fireEvent.change(screen.getByLabelText("模型标识"), { target: { value: "gpt-4o" } });
  expect((screen.getByRole("button", { name: /保存/ }) as HTMLButtonElement).disabled).toBe(false);
});
```

`stays on the form without dismissing when the apply fails` 与 `applies the config and finishes` 都需要先满足门禁才能点到保存，因此一并改写：

```tsx
it("stays on the form without dismissing when the apply fails", async () => {
  // 单写清单：失败由宿主以 RPC 错误抛出。向导内联显示，不前进、不发 dismiss。
  const call = vi.fn(async (method: string) => {
    if (method === "onboarding/test") return { ok: true, reason: null };
    if (method === "onboarding/apply") throw new Error("写入失败");
    return { ok: true };
  });
  const { onDone } = renderWizard(call);
  fireEvent.click(screen.getByRole("button", { name: /开始/ }));
  fireEvent.click(screen.getByRole("button", { name: "openai" }));
  fireEvent.click(screen.getByRole("button", { name: /下一步/ }));
  fireEvent.change(await screen.findByLabelText("API 密钥"), { target: { value: "sk-x" } });
  fireEvent.change(screen.getByLabelText("模型标识"), { target: { value: "gpt-4o" } });
  fireEvent.click(screen.getByRole("button", { name: /测试连接/ }));
  await screen.findByText("连接正常");
  fireEvent.click(screen.getByRole("button", { name: /保存/ }));
  expect(await screen.findByText("写入失败")).toBeTruthy();
  expect(onDone).not.toHaveBeenCalled();
  expect(call).not.toHaveBeenCalledWith("onboarding/dismiss", {});
});

it("applies the config after a passing test and finishes", async () => {
  const call = vi.fn(async (method: string) => {
    if (method === "onboarding/test") return { ok: true, reason: null };
    if (method === "onboarding/apply") return { ok: true, name: "gpt-4o" };
    return { ok: true };
  });
  const { onDone } = renderWizard(call);
  fireEvent.click(screen.getByRole("button", { name: /开始/ }));
  fireEvent.click(screen.getByRole("button", { name: "openai" }));
  fireEvent.click(screen.getByRole("button", { name: /下一步/ }));
  fireEvent.change(await screen.findByLabelText("API 密钥"), { target: { value: "sk-x" } });
  fireEvent.change(screen.getByLabelText("模型标识"), { target: { value: "gpt-4o" } });
  fireEvent.click(screen.getByRole("button", { name: /测试连接/ }));
  await screen.findByText("连接正常");
  fireEvent.click(screen.getByRole("button", { name: /保存/ }));
  await waitFor(() => expect(onDone).toHaveBeenCalledWith({ saved: true }));
  // 地址留空 = 用 provider 默认地址，由宿主补齐；向导不再回填。
  expect(call).toHaveBeenCalledWith("onboarding/apply", {
    provider: "openai",
    model: "gpt-4o",
    api_url: "",
    api_key: "sk-x",
  });
});

/// 测通之后再改字段，必须回到「未测试」：否则用户测的是 A 配置，
/// 存进去的却是 B 配置——门禁形同虚设。
it("invalidates a passing test when a field changes", async () => {
  const call = vi.fn(async (method: string) => {
    if (method === "onboarding/test") return { ok: true, reason: null };
    return { ok: true };
  });
  renderWizard(call);
  fireEvent.click(screen.getByRole("button", { name: /开始/ }));
  fireEvent.click(screen.getByRole("button", { name: "openai" }));
  fireEvent.click(screen.getByRole("button", { name: /下一步/ }));
  fireEvent.change(await screen.findByLabelText("API 密钥"), { target: { value: "sk-x" } });
  fireEvent.change(screen.getByLabelText("模型标识"), { target: { value: "gpt-4o" } });
  fireEvent.click(screen.getByRole("button", { name: /测试连接/ }));
  await screen.findByText("连接正常");
  expect((screen.getByRole("button", { name: /保存/ }) as HTMLButtonElement).disabled).toBe(false);
  fireEvent.change(screen.getByLabelText("模型标识"), { target: { value: "gpt-4.1" } });
  expect((screen.getByRole("button", { name: /保存/ }) as HTMLButtonElement).disabled).toBe(true);
});
```

注意：`onboarding/test` 本身仍由服务端用 `settings_from_params` 校验（空 model 会被拒），所以「未填模型时连测试都点不动」是前端多一层保护，不是唯一防线。

- [ ] **Step 2: 运行测试，确认失败**

Run: `cd desktop && npx vitest run src/components/OnboardingWizard.test.tsx`
Expected: FAIL

- [ ] **Step 3: 改实现**

`OnboardingWizard.tsx` 的改动：

1. 删除 `DEFAULT_MODEL` 常量；`DEFAULT_URL` 只用于 `placeholder`。
2. 状态改为 `testState: "untested" | "testing" | "passed" | "failed"`，替换 `testReason`。
3. `input()`：

```tsx
const input = (): ApplyInput => ({
  provider,
  model: model.trim(),
  api_url: apiUrl.trim(),
  api_key: apiKey,
});
```

4. `canSave = model.trim() !== "" && apiKey.trim() !== "" && testState === "passed"`。
5. `runTest` 成功后设 `testState("passed")`，失败设 `"failed"` 并保存原因；测试按钮 `disabled={busy || !call || model.trim() === "" || apiKey.trim() === ""}`。
6. **任一字段变化都要把 `testState` 打回 `"untested"`**（provider / model / apiUrl / apiKey 的 `onChange` 里各加一行）。这行不做，门禁就是假的：用户测通 A 配置后改成 B 配置仍能保存。
7. 删掉「仍然保存」的三元；保存按钮 `disabled={busy || !call || !canSave}`。
8. 在 API 地址输入框下方加提醒：

```tsx
{looksMissingV1(provider, apiUrl) && (
  <p role="status" className="text-xs text-amber-400">
    OpenAI 兼容接口的地址通常以 /v1 结尾，请确认地址是否正确。
  </p>
)}
```

9. 字段旁标注必填，并在表单底部加一行说明：「本次只验证地址、密钥与模型标识」。

- [ ] **Step 4: 运行测试**

Run: `cd desktop && npx vitest run src/components/OnboardingWizard.test.tsx`
Expected: PASS

- [ ] **Step 5: 跑前端全量相关测试**

Run: `cd desktop && npx vitest run src/App.test.tsx src/lib/onboarding.test.ts && npx tsc --noEmit`
Expected: PASS

- [ ] **Step 6: 提交**

```bash
git add desktop/src/components/OnboardingWizard.tsx desktop/src/components/OnboardingWizard.test.tsx
git commit -m "feat(desktop): 向导必填模型且必须测试通过"
```

---

### Task 10: 设置页默认模型可见性

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/model_rpc.rs:246-292`
- Modify: `desktop/src/components/SettingsModelsTab.tsx`
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/model_rpc.rs`（内联）、`desktop/src/components/SettingsModelsTab.test.tsx`

**Interfaces:**
- Consumes: 无。
- Produces: `model/upsert` 在 `default_model == null` 时把新条目设为默认；响应带 `default_model`。

- [ ] **Step 1: 写会失败的 Rust 测试**

在 `model_rpc.rs` 的测试模块加：

```rust
#[test]
fn upsert_makes_the_first_entry_the_default() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("models.json");
    super::upsert(
        &path,
        &json!({"name":"A","provider":"anthropic","api_url":"https://a.example","model":"m","api_key":"k"}),
    )
    .unwrap();
    let catalog = yi_agent_runtime::models::load_catalog_from(&path);
    assert_eq!(catalog.default_model.as_deref(), Some("A"));
}

#[test]
fn upsert_does_not_steal_an_existing_default() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("models.json");
    super::upsert(&path, &json!({"name":"A","provider":"anthropic","api_url":"https://a.example","model":"m","api_key":"k"})).unwrap();
    super::upsert(&path, &json!({"name":"B","provider":"anthropic","api_url":"https://b.example","model":"m","api_key":"k"})).unwrap();
    let catalog = yi_agent_runtime::models::load_catalog_from(&path);
    assert_eq!(catalog.default_model.as_deref(), Some("A"));
}
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib upsert_makes_the_first_entry_the_default`
Expected: FAIL（`default_model` 是 `None`）。

- [ ] **Step 3: 实现**

在 `upsert` 的 `update_catalog` 闭包里，`match existing { ... }` 之后加：

```rust
// 第一个条目成为默认：否则"新增了模型却仍然报未配置"会让人无从理解。
// 已有默认时不抢：用户显式设过的选择必须保持。
if catalog.default_model.is_none() {
    catalog.default_model = Some(name.clone());
}
```

返回值改为（`update_catalog` 本就是泛型 `impl FnOnce(&mut ModelCatalog) -> T`，见 `models.rs:522-528`，闭包末尾加一行即可）：

```rust
let default_model = update_catalog(path, |catalog| {
    // ...既有逻辑不变...
    catalog.default_model.clone()
})
.map_err(|error| RpcError::internal(error.to_string()))?;
Ok(json!({ "ok": true, "default_model": default_model }))
```

- [ ] **Step 4: 运行测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib upsert`
Expected: PASS

- [ ] **Step 5: 加设置页警告**

在 `SettingsModelsTab.tsx` 的清单渲染之前加：

```tsx
{!loading && catalog.models.length > 0 && catalog.default_model === null && (
  <p role="status" className="mt-3 text-xs text-amber-400">
    还没有设置默认模型：新建会话会提示「未配置模型」。
    <button
      type="button"
      onClick={() => void setDefault(catalog.models[0].name)}
      className="ml-2 underline"
    >
      把「{catalog.models[0].name}」设为默认
    </button>
  </p>
)}
```

（组件的刷新函数是 `reload()`，见 `SettingsModelsTab.tsx:277,339,353`；`setDefaultModel` 从 `../lib/models` 导入。）

**测试冲突（必须先看清楚再动手）：** `SettingsModelsTab.test.tsx` 里多处用 `findByRole("alert")` 判断保存失败（如 `:139`），而新警告也带 `role="alert"`。实施时把新警告的 `role` 换成 `status`（与 `/v1` 提醒一致），避免与失败提示争用同一个可访问性角色；同时 `:181` 那个用例（`payload({ default_model: null })`）会开始渲染该警告，检查它的 `getByLabelText("全局默认模型")` 仍唯一即可。

- [ ] **Step 6: 运行前端测试**

Run: `cd desktop && npx vitest run src/components/SettingsModelsTab.test.tsx && npx tsc --noEmit`
Expected: PASS

- [ ] **Step 7: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-app-server/src/model_rpc.rs desktop/src/components/SettingsModelsTab.tsx
git commit -m "feat(model): 首个条目自动成为默认并提示缺失默认"
```

---

### Task 11: 全量验证与文档登记

**Files:**
- Modify: `docs/project-management/desktop.md`
- Modify: `docs/project-management/yi-agent-runtime.md`
- Modify: `docs/project-management/yi-agent-app-server.md`

- [ ] **Step 1: 跑全部相关测试**

```bash
cd yi-agent-rs && cargo test -p yi-agent-runtime --lib && cargo test -p yi-agent-app-server --lib && cargo test -p yi-agent --lib
cd ../desktop && npx vitest run && npx tsc --noEmit
```

Expected: 全绿。

- [ ] **Step 2: 更新项目进度文档**

在 `docs/project-management/desktop.md` 的 onboarding 两条之后追加：

```markdown
- [x] 模型配置门禁（必填 + 测试通过才落盘 + /v1 提醒）— 向导删除模型默认值回填与「仍然保存」，`onboarding/apply` 服务端先跑连接测试、失败返回 `data.code="model_test_failed"` 且零写入；`looksMissingV1` 抽到 `desktop/src/lib/models.ts` 供向导与设置页共用；运行期 `RuntimeConfig` 不再生成默认模型名，`thread/start`/`thread/resume` 在无模型时返回 `-32027`/`model_not_configured`；`model/upsert` 在无默认时把首个条目设为默认。判据：`cd desktop && npx vitest run src/components/OnboardingWizard.test.tsx src/lib/models.test.ts src/components/SettingsModelsTab.test.tsx` + `cd yi-agent-rs && cargo test -p yi-agent-runtime --lib config bootstrap && cargo test -p yi-agent-app-server --lib onboarding` — [设计](../superpowers/specs/2026-10-19-model-configuration-gate-design.md)
```

`yi-agent-runtime.md` 与 `yi-agent-app-server.md` 各加一条对应的 `[x]` 与判据（沿用同样格式）。

- [ ] **Step 3: 提交**

```bash
git add docs/project-management/
git commit -m "docs(project-management): 登记模型配置门禁完成"
```

---

## 自检记录

**Spec 覆盖检查：**
- 设计 §1（向导必填 + 测试门禁）→ Task 9。
- 设计 §2（后端自己验证）→ Task 1、Task 2。
- 设计 §3（/v1 提醒）→ Task 8、Task 9。
- 设计 §4（运行期不猜模型）→ Task 3、Task 4、Task 5、Task 6。
- 设计 §5（设置页默认条目）→ Task 10。
- 设计「测试与验收」7 条 → Task 1–11 的测试步骤逐一对应。

**占位符扫描：** 无 TBD/TODO；每个代码步骤都给出可粘贴内容。

**类型一致性：** `ModelSettings::with_default_url`（Task 1）被 Task 2 的 RPC 门禁复用；`missing_name` 的可见性在 Task 6 调整；`looksMissingV1`（Task 8）在 Task 9 复用。

**分层一致性（重要）：** 门禁只有一处，即 `rpc/onboarding.rs` 的 async `onboarding/apply` 分支。`onboarding_rpc::apply` 保持同步、只做校验 + 写盘，**不含任何等待**。计划草稿早期版本曾试图在同步函数里用 `futures::executor::block_on` 等探测结果——那会在 tokio worker 线程上 panic（"cannot block the current thread from within a runtime"），且与 async 分支各探一次、白发一轮请求。现已删除该接缝：RPC 测试的确定性由 wiremock SSE 桩提供（`rpc_tests.rs:13298` 与 `13068` 都已有现成桩），不需要注入探针。

**计划外补充（Task 3）：** 设计文档没有覆盖 `migrate_model_keys`，但它同样"凭空生成模型名"，而且跑在侧车与 CLI 的启动路径上，是绕过门禁的真实口子。若不修，Task 4 移除 `config.rs` 默认值后，旧 `.env` 用户仍会得到一条 `gpt-4o` 条目。已在计划中单列一个任务。

**其它已知风险：**
- Task 10 的 `upsert` 返回 `default_model` 后，前端若有依赖 `{ ok: true }` 精确形状的断言需同步放宽（`rpc_tests.rs` 里搜 `model/upsert`）。
- Task 3 改为不落盘后，"旧 `.env` 用户"会停在未配置状态，需要靠空状态引导把人带回向导。这是有意的：宁可让用户明确选一次模型，也不静默继承一个没确认过的名字。
- 硬门禁 vs. 探针误判：`test_connection` 用 `max_tokens = 1`，少数推理模型会拒绝该参数。设计文档已记录此取舍（不放宽门禁，改善措辞），Task 2 的错误消息由 `test_connection` 的中文原因承担。
- `thread/resume` 的拦截会让"旧会话 + 清空清单"无法恢复该会话，这是预期的：错误码 `-32027` 会把人导向设置页，恢复后重试即可。
