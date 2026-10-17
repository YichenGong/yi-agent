# Single Source of Truth for Model Config — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `~/.yi-agent/models.json` the only persisted source of model configuration, so `.env` and the catalog can never disagree.

**Architecture:** Model keys are removed from `.env`'s jurisdiction. A resolution chain of `session override > whole-package injection (CLI flags / real process env) > catalog default` is implemented once in `yi-agent-runtime` and shared by CLI and app-server. A one-shot migration imports legacy `.env` model keys into the catalog and comments them out.

**Tech Stack:** Rust 2024 (workspace `yi-agent-rs/`), `serde_json`, `dotenvy`, `tempfile` (tests), React + TypeScript + vitest (`desktop/`).

**Spec:** `docs/superpowers/specs/2026-10-17-single-source-of-truth-model-config-design.md`

## Global Constraints

- Rust edition 2024, `rust-version = 1.85`. Because of edition 2024, `std::env::set_var` / `remove_var` are `unsafe` — wrap them in `unsafe { }` exactly as the existing `EnvVarGuard` does (`yi-agent-runtime/src/config.rs:471`).
- Run commands from `yi-agent-rs/`. Never `cargo test --workspace` (OOM/deadlock risk). Use per-crate: `cargo test -p yi-agent-runtime`, `-p yi-agent-app-server`, `-p yi-agent-llm`, `-p yi-agent-web`, `-p yi-agent`.
- Before committing: `cargo fmt --all` from `yi-agent-rs/`.
- Never commit on `main`. Work in a worktree: `git worktree add .worktrees/<branch> -b <type>/<name>`.
- Commit messages: conventional commits, first line ≤72 chars, no `Co-Authored-By` line.
- The 9 model keys that must leave `.env`'s jurisdiction (exact strings):
  `YI_AGENT_PROVIDER`, `YI_AGENT_MODEL`, `YI_AGENT_MODEL_CONTEXT_LENGTH`, `MODEL_API_KEY`,
  `MODEL_API_URL`, `ANTHROPIC_API_KEY`, `ANTHROPIC_BASE_URL`, `OPENAI_API_KEY`, `OPENAI_BASE_URL`.
- Non-model keys (`YI_AGENT_MAX_TURNS`, `YI_AGENT_COMPACT_*`, `YI_AGENT_SANDBOX`, `YI_AGENT_YOLO`,
  `YI_AGENT_SYSTEM_PROMPT`, `YI_AGENT_SKILLS_CATALOG_BUDGET`, `BOCHA_API_KEY`) keep loading from
  `.env` unchanged.
- Commit order matters: each task's tests must pass before its commit.

---

## File Structure

- `yi-agent-rs/crates/yi-agent-runtime/src/config.rs` — owns the 9-key list, the pre-dotenvy
  snapshot, post-load pruning, whole-package validation, and `compact_ratio`/`model_injected` fields.
- `yi-agent-rs/crates/yi-agent-runtime/src/models.rs` — catalog entry gains `context_length`;
  resolution gains the injection tier and `compact_threshold` recomputation.
- `yi-agent-rs/crates/yi-agent-runtime/src/migrate.rs` — **new**; the one-shot `.env` → catalog migration.
- `yi-agent-rs/crates/yi-agent-runtime/src/onboarding.rs` — gains shared `import_settings_into_catalog`.
- `yi-agent-rs/crates/yi-agent-llm/src/{anthropic,openai}/client.rs` — drop the third-layer env fallback.
- `yi-agent-rs/crates/yi-agent-app-server/src/{server.rs,onboarding_rpc.rs,model_rpc.rs}` — wiring.
- `yi-agent-rs/crates/yi-agent/src/{main.rs,onboarding_prompt.rs}` — CLI onboarding now writes the catalog.
- `yi-agent-rs/crates/yi-agent-web/src/{config_meta.rs,env_file.rs,api.rs,lib.rs}` — drop model keys.
- `.env.example`, `README.md`, `README.en.md` — documentation.

Task order is dependency-driven: 1–2 build the runtime foundation, 3 opens the single-path
load, 4–6 close the bypasses, 7–8 fix the two write paths, 9–10 handle surfaces and lock it in.

---

### Task 1: Catalog entries carry `context_length`; resolution recomputes the threshold

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/models.rs`
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/config.rs`
- Test: inline `mod tests` in both files

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `ModelEntry.context_length: Option<u32>` (new public field).
  - `RuntimeConfig.compact_ratio: u32` (new public field, the raw percentage).
  - `RuntimeConfig.model_context_length: Option<u32>` (new public field, the raw value).
  - `resolve_effective` keeps its signature and now also writes `compact_threshold`.

- [ ] **Step 1: Write the failing test**

In `yi-agent-runtime/src/models.rs`'s `mod tests`, extend the `entry` helper and add a test.
Replace the existing `fn entry` body with one that includes the new field:

```rust
    fn entry(name: &str) -> ModelEntry {
        ModelEntry {
            name: name.to_string(),
            provider: ModelProvider::Anthropic,
            api_url: "https://x".into(),
            model: "claude-x".into(),
            api_key: "sk-secret-1234".into(),
            api_style: None,
            reasoning_effort: None,
            context_length: None,
        }
    }
```

Add these tests at the end of the module:

```rust
    #[test]
    fn an_entrys_context_length_recomputes_the_compact_threshold() {
        let mut cat = catalog();
        cat.models[1].context_length = Some(100_000);
        // cfg() defaults to compact_ratio 80.
        let out = resolve_effective(&cfg(), &cat, Some("B"));
        assert_eq!(out.compact_threshold, 80_000);
    }

    #[test]
    fn a_silent_context_length_leaves_the_threshold_alone() {
        let base = cfg();
        let out = resolve_effective(&base, &catalog(), Some("A"));
        assert_eq!(out.compact_threshold, base.compact_threshold);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p yi-agent-runtime --lib models::`
Expected: FAIL — `no field 'context_length' on type 'ModelEntry'`.

- [ ] **Step 3: Add the fields and the recomputation**

In `models.rs`, add the field to `ModelEntry` (after `reasoning_effort`):

```rust
    /// 该条目的上下文窗口（tokens）；`None` = 沿用 cfg 的既有阈值。
    #[serde(default)]
    pub context_length: Option<u32>,
```

In `models.rs`'s `resolve_effective`, inside the `Some(entry)` arm, after `out.reasoning_effort = ...`:

```rust
            if let Some(length) = entry.context_length {
                out.compact_threshold = length * out.compact_ratio / 100;
            }
```

In `config.rs`, add to `RuntimeConfig` (next to `compact_threshold`):

```rust
    /// 触发自动压缩的上下文占比（百分数）。保留原值以便条目改写 context_length 时重算阈值。
    pub compact_ratio: u32,
    /// 显式声明的上下文窗口（tokens）；`None` = 未声明（阈值用兜底长度算出）。
    pub model_context_length: Option<u32>,
```

In `config.rs`'s `load_inner`, the existing locals already hold `compact_ratio` and
`model_context_length`; add both to the returned `RuntimeConfig` literal:

```rust
            compact_threshold,
            compact_ratio,
            model_context_length,
```

Fix the two other `RuntimeConfig` literals: `sample_config()` (`config.rs:417`) gains
`compact_ratio: 80,` and `model_context_length: None,`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p yi-agent-runtime --lib`
Expected: PASS (all, including the two new tests).

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-runtime/src/models.rs crates/yi-agent-runtime/src/config.rs
git commit -m "feat(runtime): carry context_length on model entries"
```

---

### Task 2: Whole-package injection — snapshot before dotenvy, prune after

This is the spec's §4.3 dotenvy trap. Model keys are captured from the **real** process
environment before any `.env` file loads, then any model key that appeared *because of*
`.env` is removed from the process environment so it cannot leak to child processes.

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/config.rs`
- Test: inline `mod tests`

**Interfaces:**
- Consumes: nothing from earlier tasks (independent of Task 1).
- Produces:
  - `pub const MODEL_ENV_KEYS: &[&str]` (the 9 keys).
  - `pub struct ModelEnvSnapshot` with `pub fn take() -> Self` and `fn get(&self, key: &str) -> Option<String>`.
  - `RuntimeConfig.model_injected: bool` (new public field).
  - `pub enum PartialInjection { ... }` + `model_injection_error()` message helper.

- [ ] **Step 1: Write the failing test**

Add to `config.rs`'s `mod tests`:

```rust
    #[test]
    fn a_dotenv_model_key_never_reaches_the_config() {
        let dir = tempfile::TempDir::new().unwrap();
        let env_path = dir.path().join(".env");
        std::fs::write(&env_path, "MODEL_API_KEY=from-dotenv\n").unwrap();

        let mut env = isolated_config_env();
        env.set("YI_AGENT_WORKDIR", dir.path());

        let cfg = RuntimeConfig::load_lenient(&ConfigOverrides {
            workdir: Some(dir.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();
        assert_ne!(cfg.api_key, "from-dotenv");
        assert_eq!(std::env::var("MODEL_API_KEY").ok(), None);
    }

    #[test]
    fn a_real_process_env_key_still_injects() {
        let mut env = isolated_config_env();
        env.set("MODEL_API_KEY", "from-shell");
        env.set("YI_AGENT_PROVIDER", "openai");

        let cfg = RuntimeConfig::load_lenient(&ConfigOverrides::default()).unwrap();
        assert_eq!(cfg.api_key, "from-shell");
        assert!(cfg.model_injected);
    }

    #[test]
    fn half_an_injection_is_an_error() {
        let mut env = isolated_config_env();
        env.set("YI_AGENT_PROVIDER", "openai"); // no key anywhere
        let result = RuntimeConfig::load_lenient(&ConfigOverrides::default());
        assert!(result.is_err());
    }
```

Add `MODEL_API_KEY`, `YI_AGENT_PROVIDER` to the `isolated_config_env()` key list (see the
existing `EnvVarGuard::new([...])` at `config.rs:497`).

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p yi-agent-runtime --lib config::tests::a_dotenv_model_key_never_reaches_the_config`
Expected: FAIL — `cfg.api_key` is `"from-dotenv"` (dotenvy leaked it), and no `model_injected` field.

- [ ] **Step 3: Implement snapshot, prune, and the flag**

At the top of `config.rs`, after the imports:

```rust
/// `.env` 不再持有这些键；它们只认「真实进程环境 + CLI 覆盖」这一条整包注入路径。
pub const MODEL_ENV_KEYS: &[&str] = &[
    "YI_AGENT_PROVIDER",
    "YI_AGENT_MODEL",
    "YI_AGENT_MODEL_CONTEXT_LENGTH",
    "MODEL_API_KEY",
    "MODEL_API_URL",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_BASE_URL",
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
];

/// 加载任何 `.env` **之前**对模型键做一次进程环境快照。
///
/// 必要性：`dotenvy::from_path` 会把 `.env` 的值写进进程环境变量，之后
/// `std::env::var("MODEL_API_KEY")` 无法分辨它是用户 shell 导出的、还是刚从
/// `.env` 读进来的。快照是唯一能区分二者的时刻。
pub struct ModelEnvSnapshot {
    values: std::collections::BTreeMap<&'static str, Option<String>>,
}

impl ModelEnvSnapshot {
    pub fn take() -> Self {
        Self {
            values: MODEL_ENV_KEYS
                .iter()
                .map(|key| {
                    let value = std::env::var(key).ok().filter(|v| !v.is_empty());
                    (*key, value)
                })
                .collect(),
        }
    }

    fn get(&self, key: &str) -> Option<String> {
        self.values.get(key).cloned().flatten()
    }
}

/// 把「因 `.env` 而出现」的模型键从进程环境剔除，避免泄漏给子进程。
///
/// 判据：快照里没有、现在却有 → 它必然来自 `.env`。真实进程环境里的键不动。
fn prune_dotenv_model_keys(snapshot: &ModelEnvSnapshot) {
    for key in MODEL_ENV_KEYS {
        let from_real_env = snapshot.values.get(key).is_some_and(Option::is_some);
        if !from_real_env && std::env::var(key).ok().is_some_and(|v| !v.is_empty()) {
            unsafe {
                std::env::remove_var(key);
            }
        }
    }
}
```

Add to `RuntimeConfig`:

```rust
    /// 本次配置是否来自「整包注入」（CLI flag 或真实进程环境变量）。
    /// 为真时它压过清单的 `default_model`（会话覆盖仍在其上）。
    pub model_injected: bool,
```

Rewrite `load_inner`'s env section. Replace the current `let local_env_path = ...; let global_env_path = ...; load_env_files(...)` block with:

```rust
        // 快照必须早于任何 .env 加载（见 `ModelEnvSnapshot` 的文档）。
        let model_env = ModelEnvSnapshot::take();
        let env_path = resolve_env_path(overrides);
        load_env_files(&env_path);
        prune_dotenv_model_keys(&model_env);
```

For this task only, keep `resolve_env_path` as-is and change `load_env_files` to take a single
path (Task 3 finishes the call-site cleanup). In the same task, replace the four model locals
so they read the snapshot instead of the live env:

```rust
        let injected_provider = overrides
            .provider
            .clone()
            .or_else(|| model_env.get("YI_AGENT_PROVIDER"));
        let injected_model = overrides
            .model
            .clone()
            .or_else(|| model_env.get("YI_AGENT_MODEL"));
        let injected_url = overrides
            .api_url
            .clone()
            .or_else(|| model_env.get("MODEL_API_URL"));
        let injected_key = overrides
            .api_key
            .clone()
            .or_else(|| model_env.get("MODEL_API_KEY"));

        // 「整包」判据：注入要生效，provider 与 api_key 必须给全。给一半即报错，
        // 绝不拼出 anthropic 地址配 openai 模型名这类畸形组合。
        let injection_present = injected_provider.is_some()
            || injected_key.is_some()
            || injected_url.is_some()
            || injected_model.is_some();
        let model_injected = injected_provider.is_some() && injected_key.is_some();
        if injection_present && !model_injected {
            bail!(
                "incomplete model override: provide both provider and api_key \
                 (got provider={}, api_key={}); use the settings UI to configure a model entry instead",
                injected_provider.is_some(),
                injected_key.is_some()
            );
        }

        let provider = injected_provider.unwrap_or_else(|| "anthropic".to_string());
        let default_api_url = match provider.as_str() {
            "openai" => "https://api.openai.com/v1",
            _ => "https://api.anthropic.com",
        };
        let default_model = match provider.as_str() {
            "openai" => "gpt-4o",
            _ => "claude-sonnet-4-20250514",
        };
        let api_url = injected_url.unwrap_or_else(|| default_api_url.to_string());
        let model = injected_model.unwrap_or_else(|| default_model.to_string());
        let api_key = match injected_key {
            Some(key) => key,
            _ if allow_missing_key => String::new(),
            _ => bail!("API key required: set MODEL_API_KEY or use --api-key"),
        };
```

Delete the now-dead `injected_*`-shadowed originals. Add `model_injected,` to both
`RuntimeConfig` literals (`load_inner` and `sample_config`). In `load_inner`, the
`YI_AGENT_MODEL_CONTEXT_LENGTH` read changes from `std::env::var(...)` to
`model_env.get("YI_AGENT_MODEL_CONTEXT_LENGTH").and_then(|s| s.parse().ok())`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p yi-agent-runtime --lib config::`
Expected: PASS, including the three new tests.

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-runtime/src/config.rs
git commit -m "feat(runtime): snapshot model env keys before dotenvy loads .env"
```

---

### Task 3: One resolution chain, shared by CLI and app-server

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/models.rs`
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/config.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs` (delete `startup_config_from_catalog`)

**Interfaces:**
- Consumes: `RuntimeConfig.model_injected` (Task 2), `ModelEntry.context_length` (Task 1).
- Produces: `resolve_effective` semantics — `session override > injection > catalog default`.

- [ ] **Step 1: Write the failing test**

Add to `models.rs`'s `mod tests`:

```rust
    #[test]
    fn an_injection_beats_the_catalog_default() {
        let mut base = cfg();
        base.model_injected = true;
        base.model = "injected-model".into();
        let out = resolve_effective(&base, &catalog(), None);
        assert_eq!(out.model, "injected-model");
    }

    #[test]
    fn a_session_override_still_beats_an_injection() {
        let mut base = cfg();
        base.model_injected = true;
        base.model = "injected-model".into();
        let out = resolve_effective(&base, &catalog(), Some("B"));
        assert_eq!(out.model, "model-b");
    }
```

`cfg()` needs `model_injected: false` (and the Task 1 fields) added to its literal.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p yi-agent-runtime --lib models::tests::an_injection_beats_the_catalog_default`
Expected: FAIL — `out.model` is `"model-a"` (catalog default won).

- [ ] **Step 3: Implement the injection tier**

In `models.rs`, replace `effective_entry`'s body and `resolve_effective`'s match:

```rust
/// 解析 `cfg` 与清单得到本次生效条目。优先级：会话覆盖 > 整包注入 > 清单默认。
///
/// 注入存在时**不看**清单默认：用户显式给的一整包压过持久化的默认值，
/// 这正是「改了没反应」那个病的解药。
pub fn effective_entry_for<'a>(
    cfg: &RuntimeConfig,
    catalog: &'a ModelCatalog,
    session_override: Option<&str>,
) -> Option<&'a ModelEntry> {
    let by_name = |name: &str| catalog.models.iter().find(|m| m.name == name);
    if let Some(entry) = session_override.and_then(by_name) {
        return Some(entry);
    }
    if cfg.model_injected {
        return None;
    }
    catalog.default_model.as_deref().and_then(by_name)
}
```

Then in `resolve_effective`, change the match scrutinee:

```rust
    match effective_entry_for(cfg, catalog, session_override) {
```

Keep `effective_entry` as a thin wrapper so existing callers compile unchanged:

```rust
/// 无注入语境下的条目解析（启动期、`onboarding` 评估等）。等价于
/// `effective_entry_for` 且 `cfg` 无注入标记。
pub fn effective_entry<'a>(
    catalog: &'a ModelCatalog,
    session_override: Option<&str>,
) -> Option<&'a ModelEntry> {
    let by_name = |name: &str| catalog.models.iter().find(|m| m.name == name);
    session_override
        .and_then(by_name)
        .or_else(|| catalog.default_model.as_deref().and_then(by_name))
}
```

In `yi-agent/src/main.rs`, delete `startup_config_from_catalog` (its whole body and doc
comment) and its call site at line ~1221. Replace that call so the CLI resolves through the
same chain:

```rust
    let catalog = yi_agent_runtime::models::load_catalog();
    let config = yi_agent_runtime::models::resolve_effective(&config, &catalog, None);
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p yi-agent-runtime --lib && cargo test -p yi-agent --lib`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-runtime/src/models.rs crates/yi-agent/src/main.rs
git commit -m "feat: resolve models through one chain in CLI and app-server"
```

---

### Task 4: Delete the provider clients' environment fallback

The invisible third layer: `AnthropicProvider::new` / `OpenaiProvider::new` read
`ANTHROPIC_API_KEY` / `OPENAI_API_KEY` (`*_BASE_URL`) themselves, bypassing every rule above.

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/anthropic/client.rs:65-78`
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/openai/client.rs:111-124`
- Test: `yi-agent-rs/crates/yi-agent-llm/tests/integration.rs:245-262`, `tests/openai_integration.rs:268-283`

**Interfaces:**
- Consumes: nothing.
- Produces: `new()` fails with `ProviderError::Auth` when no explicit key is given, even if the env var is set.

- [ ] **Step 1: Rewrite the two env-fallback tests to assert their absence**

In `tests/integration.rs`, replace the test that begins `let result = AnthropicProvider::new(AnthropicProviderOpts {` around line 245 and the one asserting `"env key picked up"` at line 259 with:

```rust
#[test]
fn new_does_not_fall_back_to_the_anthropic_env_var() {
    // 第三层已删除：key 只能经 RuntimeConfig 单点流入。
    unsafe {
        std::env::set_var("ANTHROPIC_API_KEY", "sk-from-env");
    }
    let result = AnthropicProvider::new(AnthropicProviderOpts::default());
    unsafe {
        std::env::remove_var("ANTHROPIC_API_KEY");
    }
    assert!(result.is_err(), "the env var must no longer be consulted");
}
```

Do the same in `tests/openai_integration.rs` for `OPENAI_API_KEY`, titled
`new_does_not_fall_back_to_the_openai_env_var`.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p yi-agent-llm --test integration new_does_not_fall_back && cargo test -p yi-agent-llm --test openai_integration new_does_not_fall_back`
Expected: FAIL — `result.is_err()` is false (the env var was still consulted).

- [ ] **Step 3: Remove the fallbacks**

In `anthropic/client.rs`, change:

```rust
        let api_key = opts.api_key.ok_or_else(|| {
            ProviderError::Auth("no api key provided: configure a model entry in settings".into())
        })?;
```

In `openai/client.rs`, the same shape with the same message.

Also drop the `ANTHROPIC_BASE_URL` / `OPENAI_BASE_URL` fallbacks: change
`.or_else(|| env::var("ANTHROPIC_BASE_URL").ok())` to be removed, so the chain is
`opts.base_url.unwrap_or_else(|| DEFAULT_BASE_URL.to_string())`. Update both doc comments
(the `All fields optional — resolved with the following priority` lists) to drop the
environment-variable tier. Remove the now-unused `use std::env;` if nothing else needs it.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p yi-agent-llm`
Expected: PASS. Then confirm no other crate depended on the fallback:
`cargo test -p yi-agent-runtime --lib && cargo test -p yi-agent --lib`

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-llm/src/anthropic/client.rs crates/yi-agent-llm/src/openai/client.rs crates/yi-agent-llm/tests/integration.rs crates/yi-agent-llm/tests/openai_integration.rs
git commit -m "refactor(llm): drop the provider clients' env-var fallback"
```

---

### Task 5: The one-shot `.env` → catalog migration

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-runtime/src/migrate.rs`
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/lib.rs` (add `pub mod migrate;`)

**Interfaces:**
- Consumes: `config::MODEL_ENV_KEYS` (Task 2), `models::{ModelCatalog, ModelEntry, save_catalog_to, load_catalog_from}`.
- Produces:
  - `pub enum MigrationOutcome { NothingToDo, Migrated { entry: String }, AlreadyCategorized, EnvNotWritable { reason: String }, CatalogNotWritable { reason: String } }`
  - `pub fn migrate_model_keys(env_path: &Path, models_path: &Path) -> MigrationOutcome`

Ordering rule (critical): **write the catalog first, comment `.env` second.** Reversed, a
failed catalog write would orphan the user's key (commented out, nowhere else).

- [ ] **Step 1: Write the failing tests**

Create `yi-agent-runtime/src/migrate.rs` with only the test module plus stubs:

```rust
//! 一次性迁移：把存量 `.env` 里的活跃模型键收边进清单，并注释掉它们。

use std::path::Path;

#[derive(Debug, PartialEq, Eq)]
pub enum MigrationOutcome {
    NothingToDo,
    Migrated { entry: String },
    AlreadyCategorized,
    EnvNotWritable { reason: String },
    CatalogNotWritable { reason: String },
}

pub fn migrate_model_keys(_env_path: &Path, _models_path: &Path) -> MigrationOutcome {
    MigrationOutcome::NothingToDo
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_env(dir: &Path, text: &str) -> std::path::PathBuf {
        let path = dir.join(".env");
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn an_empty_env_is_nothing_to_do() {
        let dir = tempfile::TempDir::new().unwrap();
        let env = write_env(dir.path(), "YI_AGENT_MAX_TURNS=20\n");
        let models = dir.path().join("models.json");
        assert_eq!(migrate_model_keys(&env, &models), MigrationOutcome::NothingToDo);
        assert!(!models.exists());
    }

    #[test]
    fn an_empty_catalog_imports_and_comments_out_the_env_keys() {
        let dir = tempfile::TempDir::new().unwrap();
        let env = write_env(
            dir.path(),
            "YI_AGENT_PROVIDER=openai\nYI_AGENT_MODEL=gpt-4o\nMODEL_API_KEY=sk-x\nYI_AGENT_MAX_TURNS=20\n",
        );
        let models = dir.path().join("models.json");
        assert_eq!(
            migrate_model_keys(&env, &models),
            MigrationOutcome::Migrated { entry: "gpt-4o".into() }
        );

        let catalog = crate::models::load_catalog_from(&models);
        assert_eq!(catalog.default_model.as_deref(), Some("gpt-4o"));
        assert_eq!(catalog.models[0].api_key, "sk-x");

        let text = std::fs::read_to_string(&env).unwrap();
        assert!(text.contains("# MIGRATED: YI_AGENT_PROVIDER=openai"));
        assert!(!text.contains("\nYI_AGENT_PROVIDER=openai"));
        assert!(text.contains("YI_AGENT_MAX_TURNS=20"), "non-model keys must survive");
    }

    #[test]
    fn a_populated_catalog_is_not_overwritten() {
        let dir = tempfile::TempDir::new().unwrap();
        let models = dir.path().join("models.json");
        let mut catalog = crate::models::ModelCatalog::default();
        catalog.models.push(crate::models::ModelEntry {
            name: "keep-me".into(),
            provider: crate::models::ModelProvider::Anthropic,
            api_url: "https://a".into(),
            model: "claude-x".into(),
            api_key: "sk-keep".into(),
            api_style: None,
            reasoning_effort: None,
            context_length: None,
        });
        crate::models::save_catalog_to(&models, &catalog).unwrap();

        let env = write_env(dir.path(), "MODEL_API_KEY=sk-stale\n");
        assert_eq!(
            migrate_model_keys(&env, &models),
            MigrationOutcome::AlreadyCategorized
        );
        let after = crate::models::load_catalog_from(&models);
        assert_eq!(after.models.len(), 1);
        assert_eq!(after.models[0].name, "keep-me");
        assert!(std::fs::read_to_string(&env).unwrap().contains("# MIGRATED: MODEL_API_KEY=sk-stale"));
    }

    #[test]
    fn migration_is_idempotent() {
        let dir = tempfile::TempDir::new().unwrap();
        let env = write_env(dir.path(), "MODEL_API_KEY=sk-x\n");
        let models = dir.path().join("models.json");
        migrate_model_keys(&env, &models);
        assert_eq!(migrate_model_keys(&env, &models), MigrationOutcome::NothingToDo);
    }

    #[test]
    fn a_read_only_env_degrades_without_losing_the_catalog() {
        let dir = tempfile::TempDir::new().unwrap();
        let env = write_env(dir.path(), "MODEL_API_KEY=sk-x\n");
        let models = dir.path().join("models.json");
        let mut perms = std::fs::metadata(&env).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&env, perms).unwrap();

        match migrate_model_keys(&env, &models) {
            MigrationOutcome::EnvNotWritable { .. } => {}
            other => panic!("expected EnvNotWritable, got {other:?}"),
        }
        // The catalog write already happened; the key is not lost.
        let catalog = yi_agent_runtime::models::load_catalog_from(&models);
        assert_eq!(catalog.models[0].api_key, "sk-x");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p yi-agent-runtime --lib migrate::`
Expected: FAIL — the import case returns `NothingToDo`.

- [ ] **Step 3: Implement the migration**

Replace the stub with the real implementation:

```rust
use crate::config::MODEL_ENV_KEYS;

fn is_model_key(key: &str) -> bool {
    MODEL_ENV_KEYS.contains(&key)
}

/// 扫描 `.env`，返回活跃（未被注释）模型键的 `key -> value`。
fn active_model_keys(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some(eq) = trimmed.find('=') else { continue };
        let key = trimmed[..eq].trim();
        if is_model_key(key) {
            out.push((key.to_string(), trimmed[eq + 1..].trim().trim_matches('"').trim_matches('\'').to_string()));
        }
    }
    out
}

/// 把活跃模型键行改写为 `# MIGRATED: <原行>`，其余行逐字节保留。
fn comment_out_model_keys(text: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        let active = !trimmed.is_empty() && !trimmed.starts_with('#');
        let key = trimmed
            .find('=')
            .map(|eq| trimmed[..eq].trim())
            .unwrap_or_default();
        if active && is_model_key(key) {
            let indent_len = line.len() - line.trim_start().len();
            lines.push(format!(
                "{}# MIGRATED: {}",
                &line[..indent_len],
                trimmed
            ));
        } else {
            lines.push(line.to_string());
        }
    }
    let mut joined = lines.join("\n");
    if text.ends_with('\n') {
        joined.push('\n');
    }
    joined
}

pub fn migrate_model_keys(env_path: &Path, models_path: &Path) -> MigrationOutcome {
    let text = match std::fs::read_to_string(env_path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return MigrationOutcome::NothingToDo,
        Err(error) => return MigrationOutcome::EnvNotWritable { reason: error.to_string() },
    };
    let active = active_model_keys(&text);
    if active.is_empty() {
        return MigrationOutcome::NothingToDo;
    }

    // 1) 先落清单：只有清单为空时才导入，绝不覆盖用户既有配置。
    let existing = crate::models::load_catalog_from(models_path);
    let outcome = if existing.models.is_empty() {
        let get = |key: &str| {
            active.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone()).unwrap_or_default()
        };
        let settings = crate::onboarding::ModelSettings {
            provider: get("YI_AGENT_PROVIDER"),
            model: get("YI_AGENT_MODEL"),
            api_url: get("MODEL_API_URL"),
            api_key: get("MODEL_API_KEY"),
        };
        let provider = if settings.provider.trim().is_empty() {
            "anthropic".to_string()
        } else {
            settings.provider.clone()
        };
        let mut catalog = existing.clone();
        let name = {
            let base = if settings.model.trim().is_empty() {
                provider.clone()
            } else {
                settings.model.trim().to_string()
            };
            let mut candidate = base.clone();
            let mut n = 2u32;
            while catalog.models.iter().any(|m| m.name == candidate) {
                candidate = format!("{base}-{n}");
                n += 1;
            }
            candidate
        };
        let Some(parsed) = crate::models::ModelProvider::parse(&provider) else {
            return MigrationOutcome::CatalogNotWritable { reason: format!("unknown provider: {provider}") };
        };
        let default_url = match parsed {
            crate::models::ModelProvider::Openai => "https://api.openai.com/v1",
            crate::models::ModelProvider::Anthropic => "https://api.anthropic.com",
        };
        let api_url = if get("MODEL_API_URL").trim().is_empty() {
            default_url.to_string()
        } else {
            get("MODEL_API_URL")
        };
        let model = if get("YI_AGENT_MODEL").trim().is_empty() {
            match parsed {
                crate::models::ModelProvider::Openai => "gpt-4o".to_string(),
                crate::models::ModelProvider::Anthropic => "claude-sonnet-4-20250514".to_string(),
            }
        } else {
            get("YI_AGENT_MODEL")
        };
        let context_length = get("YI_AGENT_MODEL_CONTEXT_LENGTH").parse::<u32>().ok();
        catalog.models.push(crate::models::ModelEntry {
            name: name.clone(),
            provider: parsed,
            api_url,
            model,
            api_key: get("MODEL_API_KEY"),
            api_style: None,
            reasoning_effort: None,
            context_length,
        });
        catalog.default_model = Some(name.clone());
        if let Err(error) = crate::models::save_catalog_to(models_path, &catalog) {
            // 清单没落成 → 绝不注释 .env（否则用户的 key 凭空丢失）。
            return MigrationOutcome::CatalogNotWritable { reason: error.to_string() };
        }
        MigrationOutcome::Migrated { entry: name }
    } else {
        MigrationOutcome::AlreadyCategorized
    };

    // 2) 后注释 .env：原子写，失败只降级为 warning，不改变已落的清单。
    let rewritten = comment_out_model_keys(&text);
    if let Err(error) = atomic_write(env_path, &rewritten) {
        return MigrationOutcome::EnvNotWritable { reason: error.to_string() };
    }
    outcome
}

/// 原子写：临时文件 + rename，权限 0600（.env 含密钥）。
fn atomic_write(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(".env.{}.migrate.tmp", std::process::id()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(text.as_bytes())?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&tmp, text)?;
    }
    std::fs::rename(&tmp, path)
}
```

Add `pub mod migrate;` to `lib.rs`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p yi-agent-runtime --lib migrate::`
Expected: PASS (5 tests).

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-runtime/src/migrate.rs crates/yi-agent-runtime/src/lib.rs
git commit -m "feat(runtime): one-shot migration of .env model keys into the catalog"
```

---

### Task 6: Wire the migration into the three user-facing entry points

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs` (`run_agent`, the `web` arm)
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` (startup)

**Interfaces:**
- Consumes: `yi_agent_runtime::migrate::migrate_model_keys` (Task 5).
- Produces: a `log_migration(outcome)` helper per call site; no new public API.

- [ ] **Step 1: Write the failing test**

In `yi-agent-app-server/src/server.rs`'s tests, add a test that starting a server with a
legacy `.env` moves the keys into the injected catalog path:

```rust
    #[test]
    fn startup_migrates_legacy_model_keys_into_the_catalog() {
        let dir = tempfile::TempDir::new().unwrap();
        let env_path = dir.path().join(".env");
        std::fs::write(&env_path, "YI_AGENT_PROVIDER=openai\nMODEL_API_KEY=sk-old\n").unwrap();
        let models_path = dir.path().join("models.json");

        yi_agent_runtime::migrate::migrate_model_keys(&env_path, &models_path);

        let catalog = yi_agent_runtime::models::load_catalog_from(&models_path);
        assert_eq!(catalog.models.len(), 1);
        assert!(!std::fs::read_to_string(&env_path).unwrap().contains("\nMODEL_API_KEY=sk-old"));
    }
```

(This asserts the wiring's contract at the function the entry point calls; the entry-point
call itself is verified by the manual check in Step 4.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p yi-agent-app-server --lib startup_migrates_legacy_model_keys`
Expected: FAIL — module `yi_agent_runtime::migrate` not found (if Task 5 was skipped) or PASS
once it exists; either way the entry points are not yet wired, so continue.

- [ ] **Step 3: Wire the three sites**

Add to `yi-agent-runtime/src/migrate.rs` a logging helper:

```rust
/// 把迁移结果记成日志。触发点共用，保证三种结果都有可见的措辞。
pub fn log_outcome(outcome: &MigrationOutcome, env_path: &Path) {
    match outcome {
        MigrationOutcome::NothingToDo => {}
        MigrationOutcome::Migrated { entry } => tracing::warn!(
            entry = %entry,
            path = %env_path.display(),
            "migrated model settings from .env into models.json; the .env lines were commented out"
        ),
        MigrationOutcome::AlreadyCategorized => tracing::warn!(
            path = %env_path.display(),
            "the model keys in .env are no longer read; remove them or use the settings UI"
        ),
        MigrationOutcome::EnvNotWritable { reason } => tracing::warn!(
            path = %env_path.display(),
            %reason,
            "could not comment out the legacy model keys in .env; they are ignored regardless"
        ),
        MigrationOutcome::CatalogNotWritable { reason } => tracing::warn!(
            path = %env_path.display(),
            %reason,
            "could not migrate the legacy model keys; .env was left untouched"
        ),
    }
}
```

In `yi-agent/src/main.rs`'s `run_agent`, before the existing onboarding block:

```rust
    let global_env = config::resolve_global_env_path();
    if let Some(env_path) = global_env.as_deref() {
        let outcome = yi_agent_runtime::migrate::migrate_model_keys(
            env_path,
            &yi_agent_runtime::models::models_path(),
        );
        yi_agent_runtime::migrate::log_outcome(&outcome, env_path);
    }
```

In the `Command::Web` arm's `rt.block_on`, before `yi_agent_web::serve(...)`:

```rust
                if let Some(env_path) = config::resolve_global_env_path() {
                    let outcome = yi_agent_runtime::migrate::migrate_model_keys(
                        &env_path,
                        &yi_agent_runtime::models::models_path(),
                    );
                    yi_agent_runtime::migrate::log_outcome(&outcome, &env_path);
                }
```

In `yi-agent-app-server/src/server.rs`, at the production startup (the two sites that build
`onboarding_env_path` at `:1438` and `:1513`), add before constructing the config:

```rust
        let _ = yi_agent_runtime::migrate::log_outcome(
            &yi_agent_runtime::migrate::migrate_model_keys(
                &onboarding_env_path,
                &models_path,
            ),
            &onboarding_env_path,
        );
```

Keep it inside the lazy path that has both paths in scope; do not add it to the test
constructors (they inject paths and must stay side-effect-free).

- [ ] **Step 4: Run tests and verify manually**

Run: `cargo test -p yi-agent-runtime --lib migrate:: && cargo test -p yi-agent-app-server --lib startup_migrates`
Expected: PASS.

Manual: `HOME=/tmp/yi-agent-migrate-probe mkdir -p /tmp/yi-agent-migrate-probe/.yi-agent && printf 'MODEL_API_KEY=sk-probe\nYI_AGENT_MAX_TURNS=20\n' > /tmp/yi-agent-migrate-probe/.yi-agent/.env`, then run the binary once and confirm the `.env` has `# MIGRATED: MODEL_API_KEY=sk-probe` and that `~/.yi-agent/models.json` gained one entry.

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-runtime/src/migrate.rs crates/yi-agent/src/main.rs crates/yi-agent-app-server/src/server.rs
git commit -m "feat: migrate legacy .env model keys at the user-facing entry points"
```

---

### Task 7: Expand the shared import helper and remove project-level `.env`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/onboarding.rs` (add shared importer)
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/config.rs` (single env path; drop `is_workdir_explicit` branch)
- Modify: `yi-agent-rs/crates/yi-agent-web/src/api.rs`, `lib.rs`

**Interfaces:**
- Consumes: `models::{ModelCatalog, ModelEntry, ModelProvider, load_catalog_from, save_catalog_to}`.
- Produces:
  - `pub fn import_settings_into_catalog(models_path: &Path, settings: &ModelSettings) -> Result<String, String>`
  - `load_env_files(path: &Path)` — single-path signature (was `local, global: Option<&Path>`).

- [ ] **Step 1: Write the failing test**

In `onboarding.rs`'s tests:

```rust
    #[test]
    fn importing_settings_suffixes_on_a_name_collision() {
        let dir = tempfile::TempDir::new().unwrap();
        let models = dir.path().join("models.json");
        let settings = ModelSettings {
            provider: "openai".into(),
            model: "dup".into(),
            api_url: "https://gw/v1".into(),
            api_key: "sk-a".into(),
        };
        assert_eq!(import_settings_into_catalog(&models, &settings).unwrap(), "dup");
        assert_eq!(import_settings_into_catalog(&models, &settings).unwrap(), "dup-2");
    }

    #[test]
    fn importing_rejects_an_unknown_provider() {
        let dir = tempfile::TempDir::new().unwrap();
        let settings = ModelSettings {
            provider: "bogus".into(),
            model: "m".into(),
            api_url: "https://x".into(),
            api_key: "k".into(),
        };
        assert!(import_settings_into_catalog(&dir.path().join("models.json"), &settings).is_err());
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p yi-agent-runtime --lib importing_settings_suffixes`
Expected: FAIL — `import_settings_into_catalog` not found.

- [ ] **Step 3: Implement the shared importer and the single env path**

Move the logic from `onboarding_rpc.rs::import_into_catalog` into `onboarding.rs`, adapted to
`ModelSettings` and returning `Result<String, String>`:

```rust
/// 把一组模型设置落成清单里的一条并设为默认。重名自动加 `-2`/`-3` 后缀。
///
/// CLI 的终端引导与 app-server 的 `onboarding/apply` 共用它，保证两条写路径同源。
pub fn import_settings_into_catalog(
    models_path: &Path,
    settings: &ModelSettings,
) -> Result<String, String> {
    let Some(provider) = crate::models::ModelProvider::parse(&settings.provider) else {
        return Err(format!("unknown provider: {}", settings.provider));
    };
    let mut catalog = crate::models::load_catalog_from(models_path);
    let base = settings.model.trim().to_string();
    let mut name = base.clone();
    let mut n = 2u32;
    while catalog.models.iter().any(|m| m.name == name) {
        name = format!("{base}-{n}");
        n += 1;
    }
    catalog.models.push(crate::models::ModelEntry {
        name: name.clone(),
        provider,
        api_url: settings.api_url.trim().to_string(),
        model: settings.model.trim().to_string(),
        api_key: settings.api_key.clone(),
        api_style: None,
        reasoning_effort: None,
        context_length: None,
    });
    catalog.default_model = Some(name.clone());
    crate::models::save_catalog_to(models_path, &catalog).map_err(|e| e.to_string())?;
    Ok(name)
}
```

Then in `onboarding_rpc.rs::import_into_catalog`, delete the local implementation and call the
shared one:

```rust
fn import_into_catalog(
    models_path: &Path,
    settings: &yi_agent_runtime::onboarding::ModelSettings,
) -> Result<String, String> {
    yi_agent_runtime::onboarding::import_settings_into_catalog(models_path, settings)
}
```

In `config.rs`: change `load_env_files` to a single path and delete the branching in
`load_inner`. New signature and body:

```rust
/// 加载全局 `.env` 到进程环境变量(不覆盖已存在的)。文件不存在则静默跳过。
pub(crate) fn load_env_files(path: &Path) {
    load_one_env(path);
}
```

In `load_inner`, replace the `local_env_path`/`global_env_path` block (already edited in
Task 2) with:

```rust
        let env_path = resolve_global_env_path().unwrap_or_else(|| PathBuf::from(".yi-agent").join(".env"));
        load_env_files(&env_path);
```

Delete `resolve_env_path`, `is_workdir_explicit`, and their tests. Update the callers:
`yi-agent/src/main.rs:52` (web arm) and `:1208` now use `resolve_global_env_path()`;
`yi-agent/src/config.rs:333` loses its `resolve_env_path` re-export. In
`yi-agent-web/src/lib.rs`, `serve()` takes only `env_path` and `api.rs`'s
`AppState` drops `global_env_path`; the `/api/env` handler reads and writes that one path.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p yi-agent-runtime --lib && cargo test -p yi-agent-web --lib && cargo test -p yi-agent --lib && cargo test -p yi-agent-app-server --lib`
Expected: PASS. Fix any test that referenced the removed `resolve_env_path` by pointing it at
`resolve_global_env_path`.

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add -A crates/yi-agent-runtime crates/yi-agent-web crates/yi-agent crates/yi-agent-app-server
git commit -m "feat: keep one global .env and share the catalog importer"
```

---

### Task 8: Onboarding writes the catalog, on both paths

`onboarding/apply` today writes `.env` **and** the catalog; the CLI's terminal prompt writes
only `.env`. Both become catalog-only. Also fixes `should_offer` never seeing the catalog.

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/onboarding_rpc.rs` (`apply`)
- Modify: `yi-agent-rs/crates/yi-agent/src/onboarding_prompt.rs`
- Test: inline `mod tests` in both

**Interfaces:**
- Consumes: `onboarding::import_settings_into_catalog` (Task 7).
- Produces: `onboarding/apply` response is exactly `{ "ok": true, "name": <string> }` (no
  `env_written`, no `imported`, no `import_error`).

- [ ] **Step 1: Rewrite the tests to assert catalog-only writes**

In `onboarding_rpc.rs`'s tests, replace the body of `apply_writes_the_env_and_lands_the_catalog_entry`
with the catalog-only version below (the existing `paths()` helper gives `p.env` / `p.models`).
It reads keys whose names match the new contract, so rename it too:

```rust
    #[test]
    fn apply_writes_the_catalog_and_never_the_env() {
        let p = paths();
        let out = call(
            &p,
            "onboarding/apply",
            json!({"provider":"openai","model":"gpt-4o","api_url":"https://u","api_key":"sk-x"}),
        )
        .unwrap();
        assert_eq!(out["ok"], true);
        assert!(out.get("name").is_some());
        assert!(out.get("env_written").is_none());
        assert!(out.get("imported").is_none());
        assert!(out.get("import_error").is_none());

        assert!(
            !p.env.exists() || !std::fs::read_to_string(&p.env).unwrap().contains("MODEL_API_KEY"),
            "apply must not write model keys into .env"
        );

        let catalog = yi_agent_runtime::models::load_catalog_from(&p.models);
        assert_eq!(catalog.default_model.as_deref(), Some("gpt-4o"));
        assert_eq!(catalog.models[0].api_key, "sk-x");
    }
```

Rename `status_reports_ready_when_the_env_is_complete` to
`status_reports_ready_once_the_catalog_is_complete`; its body needs no change (it calls
`apply` then asserts `needed == false`, and `status` reads the catalog).

In `onboarding_prompt.rs`'s tests, replace the test that asserted a completed prompt writes
`.env` with this. The prompt's paths are injected, so the test drives the catalog path:

```rust
    #[test]
    fn a_completed_prompt_writes_the_catalog_not_the_env() {
        let dir = tempfile::TempDir::new().unwrap();
        let models = dir.path().join("models.json");

        // 直接断言共享导入器（`run_onboarding_prompt` 是它的终端封装）：
        let settings = yi_agent_runtime::onboarding::ModelSettings {
            provider: "openai".into(),
            model: "gpt-4o".into(),
            api_url: "https://api.openai.com/v1".into(),
            api_key: "sk-x".into(),
        };
        let name = yi_agent_runtime::onboarding::import_settings_into_catalog(&models, &settings)
            .unwrap();
        assert_eq!(name, "gpt-4o");

        let catalog = yi_agent_runtime::models::load_catalog_from(&models);
        assert_eq!(catalog.default_model.as_deref(), Some("gpt-4o"));
        assert!(!dir.path().join(".env").exists(), "the prompt must not write .env");
    }
```

Note: `run_onboarding_prompt` reads from stdin, so the test asserts its injected seam (the
shared importer) rather than driving the interactive loop; the loop itself is already covered
by the existing `should_offer` tests.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p yi-agent-app-server --lib onboarding`
Expected: FAIL — `env_written` still present.

- [ ] **Step 3: Change both write paths**

In `onboarding_rpc.rs`, replace `apply`'s body:

```rust
fn apply(
    _env_path: &Path,
    models_path: &Path,
    params: &Value,
    _fallback: &RuntimeConfig,
) -> Result<Value, RpcError> {
    let settings = settings_from_params(params)?;
    let name = yi_agent_runtime::onboarding::import_settings_into_catalog(models_path, &settings)
        .map_err(RpcError::internal)?;
    Ok(json!({ "ok": true, "name": name }))
}
```

Rename the `env_path` parameter to `_env_path` (keep the parameter for signature stability
with the dispatch site) rather than deleting it.

In `onboarding_prompt.rs`: change `run_onboarding_prompt(env_path: &Path, prefs_path: &Path)`
to `run_onboarding_prompt(models_path: &Path, prefs_path: &Path)`, replace the
`onboarding::write_model_settings(env_path, &settings)?` call with:

```rust
    let name = onboarding::import_settings_into_catalog(models_path, &settings)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
```

and change the final print to `println!("已保存模型「{name}」");`. In `should_offer`, pass the
catalog so a configured catalog suppresses the prompt:

```rust
    let env = read_env_fields(env_path).unwrap_or_default();
    let catalog = yi_agent_runtime::models::load_catalog_from(&yi_agent_runtime::models::models_path());
    matches!(assess(&env, Some(&catalog)), Assessment::Needed { .. })
```

Update `run_agent`'s call site to pass `&yi_agent_runtime::models::models_path()`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p yi-agent-app-server --lib onboarding && cargo test -p yi-agent --lib onboarding`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-app-server/src/onboarding_rpc.rs crates/yi-agent/src/onboarding_prompt.rs crates/yi-agent/src/main.rs
git commit -m "feat: onboarding writes the catalog on both paths"
```

---

### Task 9: Remove the 9 model keys from the WebUI

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-web/src/config_meta.rs`
- Test: inline `mod tests` in that file

**Interfaces:**
- Consumes: nothing.
- Produces: `ALL_VARS` contains only non-model keys; its length becomes 7.

- [ ] **Step 1: Rewrite the failing assertions**

In `config_meta.rs`'s tests, replace `assert_eq!(ALL_VARS.len(), 16);` with:

```rust
        assert_eq!(ALL_VARS.len(), 7);
        for key in yi_agent_runtime::config::MODEL_ENV_KEYS {
            assert!(
                crate::config_meta::find(key).is_none(),
                "{key} must no longer be editable here"
            );
        }
```

Add `yi-agent-runtime` to `yi-agent-web/Cargo.toml` `[dependencies]` if absent
(`yi-agent-runtime = { path = "../yi-agent-runtime" }`).

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p yi-agent-web --lib config_meta`
Expected: FAIL — length is 16 and `find("MODEL_API_KEY")` is `Some`.

- [ ] **Step 3: Delete the model-key entries**

In `config_meta.rs`, delete the 9 `VarMeta` entries for the model keys from `ALL_VARS`
(leaving `YI_AGENT_MAX_TURNS`, `YI_AGENT_WORKDIR`, `YI_AGENT_SYSTEM_PROMPT`,
`YI_AGENT_COMPACT_RATIO`, `YI_AGENT_COMPACT_USER_BUDGET_TOKENS`,
`YI_AGENT_COMPACT_TOOL_BUDGET_TOKENS`, `BOCHA_API_KEY`). Delete the now-empty
`// === Model Provider ===` group comment. Update the two "15 个环境变量" doc comments
(lines 1 and 25) to read "7 个环境变量".

In `api.rs`'s `groups()` consumer, guard against an empty group list so the UI does not
render a stray "Model Provider" section: filter groups to those with at least one var.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p yi-agent-web --lib`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-web
git commit -m "feat(web): drop model keys from the env editor"
```

---

### Task 10: Documentation, the example env, and the cross-surface lock

**Files:**
- Modify: `.env.example`
- Modify: `README.md:196-212`, `README.en.md:206-215`
- Modify: `CLAUDE.md` (config section, if it lists model keys)
- Test: `yi-agent-rs/crates/yi-agent/tests/model_config_consistency.rs` (new)

**Interfaces:**
- Consumes: every previous task.
- Produces: a regression lock proving CLI and app-server agree.

- [ ] **Step 1: Write the failing consistency test**

Create `yi-agent-rs/crates/yi-agent/tests/model_config_consistency.rs`:

```rust
//! 同一份清单 + 同一组注入，CLI 与 app-server 必须解析出同一个模型。
//! 这是防「两条腿再次分叉」的回归锁。

use yi_agent_runtime::config::{ConfigOverrides, RuntimeConfig};
use yi_agent_runtime::models::{load_catalog_from, resolve_effective, ModelCatalog, ModelEntry, ModelProvider};

fn catalog_with_default() -> ModelCatalog {
    ModelCatalog {
        models: vec![ModelEntry {
            name: "shared".into(),
            provider: ModelProvider::Openai,
            api_url: "https://gw/v1".into(),
            model: "shared-model".into(),
            api_key: "sk-shared".into(),
            api_style: None,
            reasoning_effort: None,
            context_length: None,
        }],
        default_model: Some("shared".into()),
        subagent_model: None,
    }
}

#[test]
fn cli_and_app_server_resolve_the_same_model() {
    let dir = tempfile::TempDir::new().unwrap();
    let models_path = dir.path().join("models.json");
    yi_agent_runtime::models::save_catalog_to(&models_path, &catalog_with_default()).unwrap();

    let catalog = load_catalog_from(&models_path);
    let cfg = RuntimeConfig::load_lenient(&ConfigOverrides {
        workdir: Some(dir.path().to_path_buf()),
        ..Default::default()
    })
    .unwrap();

    // app-server path (production_factory) and CLI path both funnel here.
    let resolved = resolve_effective(&cfg, &catalog, None);
    assert_eq!(resolved.provider, "openai");
    assert_eq!(resolved.model, "shared-model");
    assert_eq!(resolved.api_url, "https://gw/v1");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p yi-agent --test model_config_consistency`
Expected: FAIL to compile until Tasks 1–3 ship the new fields; then PASS. If it fails for a
real divergence, fix the divergence rather than the test.

- [ ] **Step 3: Update the docs and the example env**

In `.env.example`, delete lines 2-10 (the model block: `YI_AGENT_PROVIDER` through
`OPENAI_BASE_URL`) and replace with:

```
# === Model Provider ===
# 模型配置已迁到 ~/.yi-agent/models.json（由设置 UI 或 `model/*` RPC 管理）。
# 如需临时覆盖，用 CLI 参数或真实环境变量整包给出：--provider + --api-key（--api-url 可选）。
```

In `README.md`, replace the numbered list at lines 196-204 and the trailing sentence at 205
with:

```markdown
按优先级从低到高：

1. 默认值
2. 全局配置 `~/.yi-agent/.env`（非模型设置：轮次、压缩、沙箱等）
3. 模型配置 `~/.yi-agent/models.json`（**模型设置的唯一真相源**）
4. 命令行参数 / 真实环境变量（临时整包覆盖：`--provider` + `--api-key`）
```

Then rewrite the "常用配置" table's model rows to point at the settings UI, and add a line
stating `--model` is the model **string** and overriding it requires the whole package.
Apply the mirror change to `README.en.md:206-215` (the equivalent "Configuration" section).

In `CLAUDE.md`, if the config section enumerates model keys, replace it with the same
pointer. Verify with: `grep -n "MODEL_API_KEY\|YI_AGENT_PROVIDER" CLAUDE.md`.

- [ ] **Step 4: Run the full verification sweep**

Run, from `yi-agent-rs/`:

```bash
cargo test -p yi-agent-runtime --lib
cargo test -p yi-agent-app-server --lib
cargo test -p yi-agent-llm
cargo test -p yi-agent-web --lib
cargo test -p yi-agent --lib
cargo test -p yi-agent --test model_config_consistency
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
```

Expected: all PASS. Then run the desktop suite for the unchanged model tab:
`cd desktop && npx vitest run src/components/SettingsModelsTab.test.tsx`

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add -A .
git -C .. add .env.example README.md README.en.md CLAUDE.md
git -C .. commit -m "docs: record models.json as the single source of truth"
```

---

## Self-Review

**Spec coverage:**

| Spec section | Task |
|---|---|
| §4.1 终态表（`context_length` 入条目） | 1 |
| §4.2 解析链 + 整包 + 删第三层 + 两腿对齐 | 2, 3, 4 |
| §4.3 dotenvy 陷阱（快照 + 剔除） | 2 |
| §4.4 迁移（三情形、顺序、降级、幂等） | 5, 6 |
| §4.5 显式 workdir 统一读全局 | 7 |
| §4.6 各入口改造 | 3, 6, 7, 8, 9, 10 |
| §4.7 数据流 | 5, 6 |
| §5 错误处理与边界 | 2（半包报错）、5（只读降级、先清单后 env） |
| §6 测试策略 | 每任务内联 + 10（一致性锁、e2e） |
| §7 与既有设计的关系 | 10（文档） |
| §8 风险 | 10（README 破坏性说明） |

Spec gaps found during planning, both folded in: (a) the CLI terminal onboarding also wrote
`.env` (`onboarding_prompt.rs:106`) — covered by Task 8; (b) `should_offer` passed `None`
for the catalog so it never saw catalog readiness (`onboarding_prompt.rs:14`) — Task 8.

**Placeholder scan:** No `unimplemented!()`, TBD, or TODO steps remain. Every test and
implementation step carries its actual code. Task 8's tests assert the injected importer seam
explicitly (the interactive prompt loop reads stdin and is covered by the existing
`should_offer` tests).

**Type consistency:** `ModelEntry.context_length` (Task 1) is used in Tasks 5, 8's importer,
and 10's literal. `RuntimeConfig.model_injected` (Task 2) is used in Task 3.
`import_settings_into_catalog` has one signature, introduced in Task 7 and called in Tasks 7
and 8. `load_env_files(&Path)` is single-path from Task 2, finalized in Task 7.
`MODEL_ENV_KEYS` is defined in Task 2 and consumed in Tasks 5 and 9.

**Known ordering hazard:** Task 2 temporarily leaves `resolve_env_path` in place so Task 2's
tests pass before Task 7 removes it. Do not collapse Tasks 2 and 7.
