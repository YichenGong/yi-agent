# OpenAI base_url 含 /v1 语义对齐 + 设置页模型测试按钮 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 openai provider 的 base_url 语义对齐 OpenAI 官方约定（含 `/v1`），并在桌面设置页提供「保存前测试当前草稿」按钮，同时精确迁移旧默认值、对可疑地址给出非阻塞提示。

**Architecture:** 三块相互独立、可分别测试的改动：① 把 openai 的「补 `/v1`」职责从路径常量移到 base_url（默认值含 `/v1`，路径常量去掉 `/v1`），anthropic 完全不动；② 新增 `model/test` RPC，服务端解析密钥（草稿值或按条目名取）后复用既有 `test_connection`，桌面端在编辑面板提供按钮；③ `models.rs` 加载期做一对一的旧默认值精确迁移（仅内存），设置页对「openai 且 URL 不含 /v1」的条目加非阻塞提示。

**Tech Stack:** Rust（yi-agent-llm / yi-agent-runtime / yi-agent-app-server，wiremock + tokio 测试）、TypeScript + React + Vitest（desktop）、JSON-RPC over WebSocket。

## Global Constraints

- **严禁在 `main` 分支直接改动或提交**；本计划全部在 worktree `.worktrees/feat/openai-base-url-v1`（分支 `feat/openai-base-url-v1`）内执行。
- 提交信息用 conventional commits（`feat:` / `fix:` / `docs:` / `test:`），首行 ≤72 字符，**不写** `Co-Authored-By`。
- Rust 提交前必须 `cd yi-agent-rs && cargo fmt --all`。
- 测试优先按 crate 跑，避免 `cargo test --workspace`：如 `cargo test -p yi-agent-llm`、`cargo test -p yi-agent-runtime`、`cargo test -p yi-agent-app-server`。
- 默认测试只跑 mock（wiremock），无需真实 API key。
- **openai 的 base_url 语义 = 含版本段的 API 根**（如 `https://api.openai.com/v1`）；**anthropic 保持不含版本段**（如 `https://api.anthropic.com`）。两者不同是生态事实，不统一。
- **不引入**「末尾没有 `/v1` 就自动补」的隐式改写。
- 明文 API key 绝不离开服务端：桌面端只出掩码，`model/test` 不回传 key。

---

### Task 1: openai base_url 语义对齐（provider 层）

把「补 `/v1`」的职责从路径常量移到 base_url，并同步 openai 默认值。

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/openai/client.rs:25`（`DEFAULT_BASE_URL`）
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/openai/client.rs:185-186`（path 常量）
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/openai/client.rs:33-46`（单测模块，加断言）
- Test: `yi-agent-rs/crates/yi-agent-llm/src/openai/client.rs`（同文件 `mod tests`）

**Interfaces:**
- Consumes: 无（本任务是最底层）。
- Produces: `DEFAULT_BASE_URL = "https://api.openai.com/v1"`；openai 请求路径常量为 `"/chat/completions"` 与 `"/responses"`。后续任务依赖「base_url 含 /v1、路径不含 /v1」这一约定。

- [ ] **Step 1: 写失败测试**

在 `client.rs` 的 `mod tests`（当前 L33-46）里，把 `use super::{...}` 改为同时引入 `DEFAULT_BASE_URL`，并新增两条断言：

```rust
#[cfg(test)]
mod tests {
    use super::{DEFAULT_BASE_URL, DEFAULT_TIMEOUT_SECS, TRANSIENT_RETRY_ATTEMPTS};

    #[test]
    fn default_stream_timeout_is_five_minutes() {
        assert_eq!(DEFAULT_TIMEOUT_SECS, 300);
    }

    #[test]
    fn transient_rejection_retry_budget_is_bounded() {
        assert_eq!(TRANSIENT_RETRY_ATTEMPTS, 2);
    }

    /// base_url 语义 = 含版本段的 API 根：默认值必须自带 `/v1`,
    /// 否则拼出的路径会缺 `/v1`（这正是本次要修的老问题）。
    #[test]
    fn default_base_url_carries_the_version_segment() {
        assert_eq!(DEFAULT_BASE_URL, "https://api.openai.com/v1");
        assert!(DEFAULT_BASE_URL.ends_with("/v1"));
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-llm --lib openai::client::tests::default_base_url_carries_the_version_segment`
Expected: FAIL，断言 `"https://api.openai.com"` != `"https://api.openai.com/v1"`。

- [ ] **Step 3: 改默认值 + 路径常量**

`client.rs:25`：

```rust
const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
```

`client.rs:184-187`（`post_channel` 里的 `let path = ...`）：

```rust
        let path = match channel {
            Channel::Chat => "/chat/completions",
            Channel::Responses => "/responses",
        };
```

同时更新 `client.rs:275` 的注释（提到 `/v1/responses` 的那句），改为描述「网关无 responses 路由」而不绑定具体路径：

```rust
        // 网关无 /responses 路由：缓存 chat，并用同一逻辑请求重投一次。
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-llm --lib openai::client`
Expected: PASS（`default_base_url_carries_the_version_segment` 通过，其余既有单测仍通过）。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-llm/src/openai/client.rs
git commit -m "feat(llm): openai base_url 含版本段，路径常量去掉 /v1"
```

---

### Task 2: 修正 openai 集成测试的 base_url（含尾斜杠用例）

Task 1 后，集成测试里 `base_url: Some(server.uri())` 会拼出 `/chat/completions`（缺 `/v1`），mock 的路径匹配不到 → 必须把测试的 base_url 补上 `/v1`。

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-llm/tests/openai_integration.rs`（helper L38-39、L51-52、L526-527、L553-554；尾斜杠用例 L315-316）
- Modify: `yi-agent-rs/crates/yi-agent-llm/tests/openai_responses.rs`（helper L35-36、L50-51）
- Test: 同上两个文件

**Interfaces:**
- Consumes: Task 1 的约定（base_url 含 `/v1`、路径不含 `/v1`）。
- Produces: 测试基线恢复绿色；后续任务不再改这两个文件。

- [ ] **Step 1: 改所有 helper 的 base_url**

在 `openai_integration.rs` 中，4 处 `base_url: Some(server.uri())`（L39、L52、L527、L554）统一改为：

```rust
        base_url: Some(format!("{}/v1", server.uri())),
```

在 `openai_responses.rs` 中，2 处 `base_url: Some(server.uri())`（L36、L51）同样改为：

```rust
        base_url: Some(format!("{}/v1", server.uri())),
```

- [ ] **Step 2: 改尾斜杠用例**

`openai_integration.rs:315-316` 的 `trims_trailing_slash_in_base_url` 用例，把

```rust
        base_url: Some(format!("{}/", server.uri())),
```

改为（同时验证「含 /v1 + 尾斜杠」被正确去尾斜杠）：

```rust
        base_url: Some(format!("{}/v1/", server.uri())),
```

- [ ] **Step 3: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-llm --test openai_integration && cargo test -p yi-agent-llm --test openai_responses`
Expected: 两个文件全部 PASS。

- [ ] **Step 4: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-llm/tests/openai_integration.rs yi-agent-rs/crates/yi-agent-llm/tests/openai_responses.rs
git commit -m "test(llm): openai 集成测试 base_url 补 /v1"
```

---

### Task 3: models.rs 加载期精确迁移（旧 openai 默认值 → 含 /v1）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/models.rs`（新增纯函数 + 在 `load_catalog_from` L105 调用 + 单测）
- Test: `yi-agent-rs/crates/yi-agent-runtime/src/models.rs`（文件内 `mod tests`）

**Interfaces:**
- Consumes: `ModelProvider`、`ModelEntry`、`ModelCatalog`（本文件已有）。
- Produces:
  - `const LEGACY_OPENAI_BASE_URL: &str = "https://api.openai.com";`
  - `const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";`
  - `fn migrate_openai_api_url(entry: &mut ModelEntry)`：仅当 `provider == Openai` 且 `api_url.trim_end_matches('/') == LEGACY_OPENAI_BASE_URL` 时，把 `api_url` 设为 `OPENAI_BASE_URL`；其余不动。

- [ ] **Step 1: 写失败测试**

在 `models.rs` 文件末尾的 `mod tests` 中新增（若已有同名 helper 请复用其风格）：

```rust
    /// 加载期精确迁移：只有「openai 且恰为旧默认值」才改写；其余一律不动。
    #[test]
    fn legacy_openai_default_url_is_migrated_to_v1() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        std::fs::write(
            &path,
            r#"{"models":[
                {"name":"legacy","provider":"openai","api_url":"https://api.openai.com","model":"m","api_key":"k"},
                {"name":"legacy-slash","provider":"openai","api_url":"https://api.openai.com/","model":"m","api_key":"k"},
                {"name":"custom","provider":"openai","api_url":"https://llmapi.horizon.auto","model":"m","api_key":"k"},
                {"name":"already","provider":"openai","api_url":"https://api.openai.com/v1","model":"m","api_key":"k"},
                {"name":"anth","provider":"anthropic","api_url":"https://api.openai.com","model":"m","api_key":"k"}
            ]}"#,
        )
        .unwrap();

        let catalog = load_catalog_from(&path);
        let url_of = |name: &str| {
            catalog
                .models
                .iter()
                .find(|e| e.name == name)
                .unwrap()
                .api_url
                .clone()
        };
        assert_eq!(url_of("legacy"), "https://api.openai.com/v1");
        assert_eq!(url_of("legacy-slash"), "https://api.openai.com/v1");
        // 自定义地址绝不改写——这正是方案 A 的立场。
        assert_eq!(url_of("custom"), "https://llmapi.horizon.auto");
        // 已是新默认值：幂等，不变。
        assert_eq!(url_of("already"), "https://api.openai.com/v1");
        // anthropic 条目零影响。
        assert_eq!(url_of("anth"), "https://api.openai.com");
    }

    /// 迁移只在内存里发生，不写回文件：读一次不该改用户的数据。
    #[test]
    fn migration_does_not_rewrite_the_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        let original = r#"{"models":[{"name":"legacy","provider":"openai","api_url":"https://api.openai.com","model":"m","api_key":"k"}]}"#;
        std::fs::write(&path, original).unwrap();

        let _ = load_catalog_from(&path);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime --lib models::tests::legacy_openai_default_url_is_migrated_to_v1`
（cargo test 只接受单个过滤串，第二个用例随 `models::tests` 一并验证。）
Expected: FAIL（`legacy` 读出 `https://api.openai.com`；`migration_does_not_rewrite_the_file` 本就不写文件故 PASS，作为回归锁）。

- [ ] **Step 3: 实现迁移**

在 `models.rs` 顶部常量区（`use` 之后）加：

```rust
/// 旧版 openai 默认值（不含版本段）。仅这一精确值会被迁移，自定义地址不动。
const LEGACY_OPENAI_BASE_URL: &str = "https://api.openai.com";
/// 新版 openai 默认值：含版本段的 API 根（对齐 OpenAI 官方 SDK 约定）。
const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

/// 加载期精确迁移：把「恰为旧默认值的 openai 条目」改写为含 `/v1` 的新默认值。
///
/// 只认一对一映射（旧默认值 → 新默认值），不做任何启发式猜测；自定义地址
/// （如网关根 `https://llmapi.horizon.auto`）一律不动，由设置页提示用户自查。
fn migrate_openai_api_url(entry: &mut ModelEntry) {
    if entry.provider == ModelProvider::Openai
        && entry.api_url.trim_end_matches('/') == LEGACY_OPENAI_BASE_URL
    {
        entry.api_url = OPENAI_BASE_URL.to_string();
    }
}
```

在 `load_catalog_from` 的条目入表处（当前 L124-128，`Ok(entry) if ... => { catalog.models.push(entry); }`）改为先迁移再入表：

```rust
                Ok(mut entry)
                    if !entry.name.trim().is_empty()
                        && !entry.api_url.trim().is_empty()
                        && !entry.model.trim().is_empty() =>
                {
                    migrate_openai_api_url(&mut entry);
                    catalog.models.push(entry);
                }
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime --lib models::tests`
Expected: 全部 PASS（含既有 models 测试）。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-runtime/src/models.rs
git commit -m "feat(runtime): 旧 openai 默认地址加载期精确迁移为含 /v1"
```

---

### Task 4: 同步其余 4 处 openai 默认值

openai 默认值在仓库里共 5 处（Task 1 改了 client 层）。本任务同步剩余 4 处，避免「引导填的默认值」与「代码兜底默认值」不一致。

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/config.rs:241`（openai 默认）+ L952 断言
- Modify: `yi-agent-rs/crates/yi-agent-web/src/config_meta.rs:94`（`OPENAI_BASE_URL` default）
- Modify: `yi-agent-rs/crates/yi-agent/src/onboarding_prompt.rs:20`（CLI 引导 `provider_defaults`）+ L150 断言
- Modify: `yi-agent-rs/crates/yi-agent/tests/common/mod.rs:97,129`（e2e 测试默认 api_url）
- Modify: `desktop/src/components/OnboardingWizard.tsx:23`（桌面向导 `DEFAULT_URL`）
- Test: `yi-agent-rs/crates/yi-agent-runtime/src/config.rs`（L952）、`yi-agent-rs/crates/yi-agent/src/onboarding_prompt.rs`（L150）、`desktop/src/components/OnboardingWizard.test.tsx:47`

**Interfaces:**
- Consumes: Task 1/3 确立的新默认值 `https://api.openai.com/v1`。
- Produces: 5 处默认值一致。

- [ ] **Step 1: 改 Rust 三处 + 断言**

`config.rs:241`：

```rust
            "openai" => "https://api.openai.com/v1",
```

`config.rs:952` 断言：

```rust
        assert_eq!(config.api_url, "https://api.openai.com/v1");
```

`config_meta.rs:94`：

```rust
        default: Some("https://api.openai.com/v1"),
```

`onboarding_prompt.rs:20`：

```rust
        ("https://api.openai.com/v1", "gpt-4o")
```

同一文件 L150 的断言同步改为：

```rust
            ("https://api.openai.com/v1", "gpt-4o")
```

`crates/yi-agent/tests/common/mod.rs` L97 与 L129 的 e2e 默认 api_url 同步改为：

```rust
            ("https://api.openai.com/v1", "gpt-4o")
```

- [ ] **Step 2: 改桌面向导默认值 + 断言**

`OnboardingWizard.tsx:23`：

```tsx
  openai: "https://api.openai.com/v1",
```

`OnboardingWizard.test.tsx:47` 的期望值改为：

```tsx
      api_url: "https://api.openai.com/v1",
```

- [ ] **Step 3: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime --lib config && cargo test -p yi-agent --lib onboarding_prompt`
Run: `cd desktop && npx vitest run src/components/OnboardingWizard.test.tsx`
Expected: 全部 PASS。

- [ ] **Step 4: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-runtime/src/config.rs yi-agent-rs/crates/yi-agent-web/src/config_meta.rs yi-agent-rs/crates/yi-agent/src/onboarding_prompt.rs yi-agent-rs/crates/yi-agent/tests/common/mod.rs desktop/src/components/OnboardingWizard.tsx desktop/src/components/OnboardingWizard.test.tsx
git commit -m "feat: 同步 openai 默认地址为含 /v1（config/web/cli/desktop/e2e）"
```

---

### Task 5: 服务端 `model/test` RPC

新增 `model/test`：解析密钥（草稿值或按条目名取已有明文 key），复用 `test_connection`，回 `{ ok, reason }`。

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/model_rpc.rs`（新增 `resolve_test_settings` 辅助 + 单测）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:2688-2712`（新增 async `model/test` 分支）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/model_rpc.rs`（文件内 `mod tests`）

**Interfaces:**
- Consumes: `yi_agent_runtime::models::{load_catalog_from, ModelCatalog}`、`yi_agent_runtime::onboarding::{ModelSettings, test_connection, validate_settings}`、`crate::protocol::RpcError`。
- Produces:
  - `pub fn resolve_test_settings(path: &Path, params: &Value) -> Result<yi_agent_runtime::onboarding::ModelSettings, RpcError>`：从 `params` 读 `provider/model/api_url`；密钥优先取 `api_key`（非空字符串），否则按 `name` 从 `load_catalog_from(path)` 取该条目的 `api_key`；两者都拿不到 → `RpcError::invalid_model("请填写 API 密钥")`；最终经 `validate_settings` 校验。

- [ ] **Step 1: 写失败测试**

在 `model_rpc.rs` 的 `mod tests` 中新增（复用该文件既有的 `catalog_path()` / `call()` 风格）：

```rust
    /// 显式给出 api_key 时用它——新增模型/改过 key 的场景。
    #[test]
    fn test_settings_prefer_explicit_api_key() {
        let (dir, path) = catalog_path();
        let _ = dir;
        let params = json!({
            "provider": "openai",
            "model": "m",
            "api_url": "https://x/v1",
            "api_key": "sk-draft",
        });
        let settings = resolve_test_settings(&path, &params).unwrap();
        assert_eq!(settings.api_key, "sk-draft");
        assert_eq!(settings.api_url, "https://x/v1");
    }

    /// 没给 api_key 时按条目名取已存明文 key——编辑但没改 key 的场景。
    #[test]
    fn test_settings_fall_back_to_named_entry_key() {
        let (_dir, path) = catalog_path();
        call(
            &path,
            "model/upsert",
            json!({"name":"A","provider":"openai","api_url":"https://a/v1","model":"m","api_key":"sk-stored"}),
        )
        .unwrap();

        let params = json!({
            "provider": "openai",
            "model": "m",
            "api_url": "https://a/v1",
            "name": "A",
        });
        let settings = resolve_test_settings(&path, &params).unwrap();
        assert_eq!(settings.api_key, "sk-stored");
    }

    /// 既没 api_key 也没可命中的 name → 结构化拒绝，绝不用空 key 去探测。
    #[test]
    fn test_settings_reject_when_no_key_available() {
        let (_dir, path) = catalog_path();
        let params = json!({
            "provider": "openai",
            "model": "m",
            "api_url": "https://a/v1",
            "name": "missing",
        });
        assert!(resolve_test_settings(&path, &params).is_err());
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib model_rpc::tests::test_settings`
Expected: 编译失败 / FAIL（`resolve_test_settings` 未定义）。

- [ ] **Step 3: 实现 `resolve_test_settings`**

在 `model_rpc.rs` 中新增（放在 `settings` 相关逻辑附近；`use std::path::Path` 该文件已有）：

```rust
/// 把 `model/test` 的入参解析成一份可探测的 [`ModelSettings`]。
///
/// 密钥解析是本函数的核心：桌面端的 key 框不回填（明文从不经过桌面端），
/// 因此「编辑已有条目但没改 key」时，只能由服务端按 `name` 取已存明文 key。
/// 两者都拿不到时明确拒绝，绝不用空 key 去发一次注定 401 的探测。
pub fn resolve_test_settings(
    path: &Path,
    params: &Value,
) -> Result<yi_agent_runtime::onboarding::ModelSettings, RpcError> {
    let provider = string_field(params, "provider").unwrap_or_default();
    let model = string_field(params, "model").unwrap_or_default();
    let api_url = string_field(params, "api_url").unwrap_or_default();

    let explicit = string_field(params, "api_key").filter(|k| !k.is_empty());
    let api_key = match explicit {
        Some(key) => key,
        None => {
            let name = string_field(params, "name").unwrap_or_default();
            let stored = load_catalog_from(path)
                .models
                .into_iter()
                .find(|e| e.name == name)
                .map(|e| e.api_key)
                .filter(|k| !k.is_empty());
            stored.ok_or_else(|| RpcError::invalid_model("请填写 API 密钥"))?
        }
    };

    let settings = yi_agent_runtime::onboarding::ModelSettings {
        provider,
        model,
        api_url,
        api_key,
    };
    yi_agent_runtime::onboarding::validate_settings(&settings)
        .map_err(|e| RpcError::invalid_model(e.to_string()))?;
    Ok(settings)
}
```

确认 `model_rpc.rs` 顶部已 `use yi_agent_runtime::models::load_catalog_from;`（若只 import 了 `models_path`，在此补上）。

- [ ] **Step 4: 加 server.rs 的 `model/test` 分支**

在 `server.rs` 的 `"model/upsert" | ... | "model/importEnv"` 分支之后（L2712 之后）新增：

```rust
                    // 探测只发一次最小请求、**不落盘**：与 `onboarding/test` 同档
                    // （触及密钥，需 Control），且因探测是 async 而单独开分支。
                    "model/test" => {
                        if client_scope < Scope::Control {
                            write_response(
                                &hub,
                                &client,
                                err_response(id, RpcError::insufficient_scope(Scope::Control)),
                            )
                            .await?;
                            continue;
                        }
                        let settings =
                            match crate::model_rpc::resolve_test_settings(&models_path, &req.params) {
                                Ok(s) => s,
                                Err(error) => {
                                    write_response(&hub, &client, err_response(id, error)).await?;
                                    continue;
                                }
                            };
                        let outcome = yi_agent_runtime::onboarding::test_connection(&settings).await;
                        let value = serde_json::json!({
                            "ok": outcome.ok,
                            "reason": outcome.reason,
                        });
                        write_response(&hub, &client, ok_response(id, value)).await?;
                    }
```

- [ ] **Step 5: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib model_rpc::tests`
Expected: PASS。

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/model_rpc.rs yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): 新增 model/test RPC（服务端解析密钥后探测）"
```

---

### Task 6: 桌面端 `testModel` 封装 + 设置页「测试」按钮

**Files:**
- Modify: `desktop/src/lib/models.ts`（新增 `testModel`）
- Modify: `desktop/src/components/SettingsModelsTab.tsx`（编辑面板加「测试」按钮 + 状态）
- Test: `desktop/src/lib/models.test.ts`、`desktop/src/components/SettingsModelsTab.test.tsx`

**Interfaces:**
- Consumes: Task 5 的 `model/test` RPC（入参 `{provider, model, api_url, api_key?, name?}`，回包 `{ok, reason}`）。
- Produces:
  - `export async function testModel(rpc: ModelRpc, input: { provider: string; model: string; api_url: string; api_key?: string; name?: string }): Promise<{ ok: boolean; reason: string | null }>`

- [ ] **Step 1: 写失败的 lib 测试**

在 `desktop/src/lib/models.test.ts` 中新增（沿用该文件既有的静态 import 风格——需在顶部 import 列表加入 `testModel`）：

```ts
describe("testModel", () => {
  it("sends api_key only when explicitly provided", async () => {
    const rpc = vi.fn().mockResolvedValue({ ok: true, reason: null });

    await testModel(rpc, {
      provider: "openai",
      model: "m",
      api_url: "https://x/v1",
      api_key: "sk-draft",
    });
    expect(rpc).toHaveBeenCalledWith(
      "model/test",
      expect.objectContaining({ api_key: "sk-draft" }),
    );
  });

  it("sends name instead of api_key when no key is given", async () => {
    const rpc = vi.fn().mockResolvedValue({ ok: true, reason: null });

    await testModel(rpc, {
      provider: "openai",
      model: "m",
      api_url: "https://x/v1",
      name: "A",
    });
    const params = rpc.mock.calls[0][1] as Record<string, unknown>;
    expect(params.name).toBe("A");
    expect("api_key" in params).toBe(false);
  });

  it("treats a missing ok field as failure", async () => {
    const rpc = vi.fn().mockResolvedValue({});
    await expect(
      testModel(rpc, { provider: "openai", model: "m", api_url: "https://x/v1", name: "A" }),
    ).resolves.toEqual({ ok: false, reason: null });
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/lib/models.test.ts`
Expected: FAIL（`testModel` 未导出）。

- [ ] **Step 3: 实现 `testModel`**

在 `desktop/src/lib/models.ts` 末尾（`isModelNotFound` 之前或之后均可）新增：

```ts
/**
 * 用当前草稿值（未保存也能测）发一次最小探测。
 *
 * `api_key` 省略即「让服务端按 `name` 取已存明文 key」——桌面端从不持有明文，
 * 与 `upsertModel` 的「省略即保留」同一条安全约定。回包 `ok/reason` 缺字段时
 * 保守地当作失败，避免把「宿主少发字段」误报成连接成功。
 */
export async function testModel(
  rpc: ModelRpc,
  input: { provider: string; model: string; api_url: string; api_key?: string; name?: string },
): Promise<{ ok: boolean; reason: string | null }> {
  const params: Record<string, unknown> = {
    provider: input.provider,
    model: input.model,
    api_url: input.api_url,
  };
  if (input.api_key !== undefined) params.api_key = input.api_key;
  else if (input.name !== undefined) params.name = input.name;
  const result = await rpc<{ ok?: unknown; reason?: unknown }>("model/test", params);
  return {
    ok: result?.ok === true,
    reason: typeof result?.reason === "string" ? result.reason : null,
  };
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/lib/models.test.ts`
Expected: PASS。

- [ ] **Step 5: 写失败的组件测试**

在 `desktop/src/components/SettingsModelsTab.test.tsx` 中新增（沿用该文件既有的 `vi.fn()` + `fireEvent` + `callsTo` 风格）：

```tsx
  it("tests with name (not api_key) when editing without changing the key", async () => {
    const call = vi.fn(async (method: string, _params: unknown) =>
      method === "model/list" ? payload() : { ok: true, reason: null },
    );
    render(<SettingsModelsTab call={call} />);
    await screen.findByText("••••1234");

    fireEvent.click(screen.getByRole("button", { name: "编辑" }));
    fireEvent.click(screen.getByRole("button", { name: "测试" }));

    await waitFor(() => expect(callsTo(call, "model/test")).toHaveLength(1));
    const params = callsTo(call, "model/test")[0][1] as Record<string, unknown>;
    expect(params.name).toBe("A");
    expect("api_key" in params).toBe(false);
    expect(await screen.findByText("连接成功")).toBeTruthy();
  });

  it("shows the server reason when the test fails", async () => {
    const call = vi.fn(async (method: string, _params: unknown) => {
      if (method === "model/list") return { models: [], default_model: null, subagent_model: null, effective: null };
      if (method === "model/test") return { ok: false, reason: "API 密钥无效或无权限" };
      return {};
    });
    render(<SettingsModelsTab call={call} />);
    fireEvent.click(await screen.findByRole("button", { name: "添加模型" }));
    fireEvent.change(screen.getByLabelText("API 地址"), { target: { value: "https://x/v1" } });
    fireEvent.click(screen.getByRole("button", { name: "测试" }));

    expect(await screen.findByText("API 密钥无效或无权限")).toBeTruthy();
  });
```

- [ ] **Step 6: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/components/SettingsModelsTab.test.tsx`
Expected: FAIL（无「测试」按钮）。

- [ ] **Step 7: 实现按钮**

在 `SettingsModelsTab.tsx`：

1) import 加 `testModel`；

```tsx
import {
  deleteModel,
  importEnvModel,
  isModelNotFound,
  listModels,
  setDefaultModel,
  setSubagentModel,
  testModel,
  upsertModel,
  type EffectiveModel,
  type ModelEntryView,
  type ModelList,
  type ModelRpc,
  type ModelUpsertInput,
} from "../lib/models";
```

2) 组件内新增测试状态：

```tsx
  /** 最近一次「测试」的结论：null = 还没测；测成功显示成功，失败显示原因。 */
  const [testResult, setTestResult] = useState<
    { ok: boolean; reason: string | null } | null
  >(null);
  const [testing, setTesting] = useState(false);
```

3) 新增处理器（放在 `saveDraft` 附近）：

```tsx
  const runTest = async () => {
    if (!rpc || !draft) return;
    setTesting(true);
    setTestResult(null);
    setActionError(null);
    try {
      // key 解析约定：改过 key 就传草稿 key，否则传条目名让服务端取已存 key。
      const result = await testModel(rpc, {
        provider: draft.provider,
        model: draft.model,
        api_url: draft.api_url,
        ...(draft.keyEdited
          ? { api_key: draft.api_key }
          : draft.original !== null
            ? { name: draft.original }
            : { api_key: draft.api_key }),
      });
      setTestResult(result);
    } catch (error) {
      setTestResult({ ok: false, reason: saveErrorText(error) });
    } finally {
      setTesting(false);
    }
  };
```

4) 使 `startAdd`/`startEdit` 清空上一次测试结论：在这两个函数的末尾各加 `setTestResult(null);`。

5) 在「保存 / 取消」那一行（L511-530 的 `div`）里，在「保存」之前插入测试按钮与其结论：

```tsx
            <button
              type="button"
              onClick={() => void runTest()}
              disabled={testing || !rpc}
              className="rounded-md border border-line-strong px-3 py-1.5 text-sm text-fg-muted hover:text-fg disabled:opacity-50"
            >
              {testing ? "测试中…" : "测试"}
            </button>
```

6) 在按钮行下方补一行结论（成功/失败都可见）：

```tsx
          {testResult !== null && (
            <p
              className={`mt-2 text-xs ${testResult.ok ? "text-green-400" : "text-red-400"}`}
            >
              {testResult.ok ? "连接成功" : (testResult.reason ?? "连接失败")}
            </p>
          )}
```

- [ ] **Step 8: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/components/SettingsModelsTab.test.tsx`
Expected: PASS（含既有用例）。

- [ ] **Step 9: 提交**

```bash
git add desktop/src/lib/models.ts desktop/src/lib/models.test.ts desktop/src/components/SettingsModelsTab.tsx desktop/src/components/SettingsModelsTab.test.tsx
git commit -m "feat(desktop): 设置页模型编辑面板新增测试按钮"
```

---

### Task 7: 设置页「openai 地址不含 /v1」非阻塞提示

**Files:**
- Modify: `desktop/src/components/SettingsModelsTab.tsx`（列表行内提示）
- Test: `desktop/src/components/SettingsModelsTab.test.tsx`

**Interfaces:**
- Consumes: `ModelEntryView`（`provider` / `api_url`）。
- Produces: 纯展示逻辑；无对外接口。

- [ ] **Step 1: 写失败测试**

在 `SettingsModelsTab.test.tsx` 新增（沿用 `vi.fn()` + 本地 `ModelList` 字面量风格）：

```tsx
  it("hints when an openai url does not end with /v1", async () => {
    const call = vi.fn(async (method: string, _params: unknown) =>
      method === "model/list"
        ? {
            models: [
              { name: "suspect", provider: "openai", api_url: "https://llmapi.horizon.auto", model: "m", has_key: true, api_key_masked: "••••1" },
              { name: "ok", provider: "openai", api_url: "https://llmapi.horizon.auto/v1", model: "m", has_key: true, api_key_masked: "••••2" },
              { name: "anth", provider: "anthropic", api_url: "https://llmapi.horizon.auto", model: "m", has_key: true, api_key_masked: "••••3" },
            ],
            default_model: null,
            subagent_model: null,
            effective: null,
          }
        : {},
    );
    render(<SettingsModelsTab call={call} />);
    await screen.findByText("suspect");

    // 只有 suspect 该被标出；ok（含 /v1）与 anth（anthropic）不提示。
    expect(await screen.findAllByText(/以 \/v1 结尾/)).toHaveLength(1);
  });
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/components/SettingsModelsTab.test.tsx`
Expected: FAIL（找不到提示文案）。

- [ ] **Step 3: 实现提示**

在 `SettingsModelsTab.tsx` 顶部加纯函数：

```tsx
/**
 * openai 兼容网关的地址通常以 `/v1` 结尾。这里只做启发式提醒，不下硬结论：
 * 含 `/v1` 前缀（如 `/openai/v1`）即视为无虞，无法判断的（如 `/api`）宁可少提示。
 */
function suspectsMissingV1(entry: ModelEntryView): boolean {
  if (entry.provider !== "openai") return false;
  return !entry.api_url.trimEnd().replace(/\/+$/, "").endsWith("/v1");
}
```

在列表 `API 地址` 那个 `<td>`（L315）里追加提示：

```tsx
                <td className="py-1.5 pr-2 font-mono text-xs text-fg-subtle">
                  {entry.api_url}
                  {suspectsMissingV1(entry) && (
                    <span className="ml-2 font-sans text-fg-muted">
                      （OpenAI 兼容接口地址通常以 /v1 结尾，请确认）
                    </span>
                  )}
                </td>
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/components/SettingsModelsTab.test.tsx`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/components/SettingsModelsTab.tsx desktop/src/components/SettingsModelsTab.test.tsx
git commit -m "feat(desktop): 对可疑的 openai 地址给出 /v1 提示"
```

---

### Task 8: 前端全量回归 + 端到端验证

**Files:**
- Test: `desktop/src/**`（全量 vitest）、`yi-agent-rs`（相关 crate 测试）

**Interfaces:**
- Consumes: Task 1-7 全部产物。
- Produces: 无新接口；产出验证证据。

- [ ] **Step 1: 跑前端全量测试**

Run: `cd desktop && npx vitest run`
Expected: 全绿。

- [ ] **Step 2: 跑 Rust 相关 crate 测试**

先确认没有其它 cargo 进程：`ps aux | grep cargo | grep -v grep`
Run: `cd yi-agent-rs && cargo test -p yi-agent-llm && cargo test -p yi-agent-runtime && cargo test -p yi-agent-app-server`
Expected: 全绿。

- [ ] **Step 3: 手动端到端（真实网关）**

1. 把 `~/.yi-agent/models.json` 里 3 条 horizon 的 `api_url` 改为 `https://llmapi.horizon.auto/v1`。
2. 启动桌面端，在「设置 → 模型」页对任一条点「测试」→ 期望「连接成功」。
3. 发起一次真实对话 → 期望正常返回（不再是 404/`/v1/v1`）。
4. 反例验证：把地址临时改成漏 `/v1` 的形式点「测试」→ 期望失败并给出可读原因。

- [ ] **Step 4: 记录验证结果并提交（如有文档更新）**

```bash
git add -A
git commit -m "test: 记录 openai base_url 语义对齐的端到端验证" || echo "无改动可提交"
```

---

### Task 9: 合并回 main

**Files:** 无（仅 git 操作）

**Interfaces:** 无。

- [ ] **Step 1: 确认 worktree 干净且测试通过**

Run: `cd .worktrees/feat/openai-base-url-v1 && git status --short`
Expected: 无未提交改动。

- [ ] **Step 2: 合并**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent
git merge --no-ff feat/openai-base-url-v1 -m "Merge branch 'feat/openai-base-url-v1'：openai base_url 含 /v1 语义对齐 + 设置页模型测试按钮"
```

- [ ] **Step 3: 清理**

```bash
git worktree remove .worktrees/feat/openai-base-url-v1
git branch -d feat/openai-base-url-v1
```

---

## 自检记录

**Spec 覆盖检查：**

| Spec 章节 | 对应任务 |
|---|---|
| §3.1 openai base_url 含 /v1（5 处默认值 + 路径常量） | Task 1（client）+ Task 4（其余 4 处） |
| §3.2 设置页测试按钮 | Task 5（RPC）+ Task 6（UI） |
| §3.3.1 精确迁移 | Task 3 |
| §3.3.2 启发式提示 | Task 7 |
| §6 测试策略（Rust 单测/集成、前端、端到端） | Task 1-3、5-8 |
| §7 影响面清单 | Task 1-7 全覆盖 |

**占位符扫描：** 无 TBD/TODO；每个代码步骤都给了完整代码。

**类型一致性检查：**
- `resolve_test_settings(path, params) -> Result<ModelSettings, RpcError>`：Task 5 定义，server.rs 分支按此调用。
- `testModel(rpc, input) -> {ok, reason}`：Task 6 定义并自用，入参字段与 Task 5 的 `resolve_test_settings` 读取字段（`provider/model/api_url/api_key?/name?`）一致。
- `suspectsMissingV1(entry)`：Task 7 定义并自用。
- `migrate_openai_api_url(&mut ModelEntry)`：Task 3 定义并自用。
- 默认值字符串在所有任务中统一为 `https://api.openai.com/v1`。