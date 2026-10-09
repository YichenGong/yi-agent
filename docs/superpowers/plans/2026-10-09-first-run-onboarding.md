# First-Run Onboarding Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give a brand-new install a first-run onboarding path that detects "no usable model config", collects provider/model/api_url/api_key, writes the global `~/.yi-agent/.env`, optionally tests the connection, and lands the config in the desktop model catalog — shared by the CLI and the desktop app.

**Architecture:** One shared Rust module `yi-agent-runtime/src/onboarding.rs` owns the readiness assessment, the line-preserving `.env` writer, the connection test, and the "onboarding finished" marker. `yi-agent-app-server` exposes it as `onboarding/*` JSON-RPC for the desktop. The CLI drives it from the TUI (interactive Q&A) and from `yi-agent run` (a readable message + non-zero exit). The desktop renders a full-screen wizard.

**Tech Stack:** Rust (workspace at `yi-agent-rs/`), `tokio`, `serde_json`, `wiremock` (dev), Tauri 2 + React 19 + TypeScript + Tailwind v4 + Vitest (`desktop/`).

## Global Constraints

- **Never commit on `main`.** Work happens in the worktree `.worktrees/first-run-onboarding` on branch `feat/first-run-onboarding`. Verify with `git branch --show-current`.
- **Run `cargo fmt --all` before every commit** (in `yi-agent-rs/`). Copy exactly: `cd yi-agent-rs && cargo fmt --all`.
- **No `Co-Authored-By` lines** in commit messages. Conventional-commits style, first line ≤ 72 chars.
- **Never log or return a raw API key.** `.env` writes are the only place a plaintext key lands. RPC responses carry masks only.
- Runtime-side model env keys (exact strings): `YI_AGENT_PROVIDER`, `YI_AGENT_MODEL`, `MODEL_API_URL`, `MODEL_API_KEY`.
- Provider values are exactly `"anthropic"` or `"openai"`.
- `preferences.json` marker key is exactly `onboarding_dismissed` (boolean).
- **Do not run `cargo test --workspace`** (OOM risk). Run per-crate: `cargo test -p yi-agent-runtime`, `-p yi-agent-app-server`, `-p yi-agent`.
- Prefer `cargo test -p <crate> --lib <test_name>` for a single test.
- **CI runs clippy with `-D warnings`** (`just ci` → `cargo clippy --all-targets --all-features -- -D warnings`). Before committing a Rust change, run `cd yi-agent-rs && cargo clippy -p <changed-crate> --all-targets -- -D warnings` and fix everything (unused imports included).
- Desktop checks: `cd desktop && npx tsc --noEmit && npx vitest run`.
- Project-management docs under `docs/project-management/` must be updated in the same branch/commit as the feature (see Task 12).

---

## File Structure

**Create:**
- `yi-agent-rs/crates/yi-agent-runtime/src/onboarding.rs` — the whole shared module (assessment, `.env` writer, connection test, marker). Split into focused submodules only if it exceeds ~400 lines; keep public surface in one file so callers have one import.
- `yi-agent-rs/crates/yi-agent-app-server/src/onboarding_rpc.rs` — thin `method`+`params` → runtime translation, path-injectable for tests (mirrors `model_rpc.rs`).
- `desktop/src/lib/onboarding.ts` — pure RPC wrappers + response types.
- `desktop/src/lib/onboarding.test.ts` — wrapper unit tests.
- `desktop/src/components/OnboardingWizard.tsx` — the wizard.
- `desktop/src/components/OnboardingWizard.test.tsx` — wizard tests.

**Modify:**
- `yi-agent-rs/crates/yi-agent-runtime/src/lib.rs` — add `pub mod onboarding;`.
- `yi-agent-rs/crates/yi-agent-runtime/src/config.rs` — add `load_lenient` (key-less load) + `is_valid_api_url` helper if not already present.
- `yi-agent-rs/crates/yi-agent-runtime/Cargo.toml` — add `wiremock = "0.6"` under `[dev-dependencies]`.
- `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs` — add `pub mod onboarding_rpc;`.
- `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` — dispatch `onboarding/*`, scope gate, `RuntimeAttachments.onboarding_env_path`, use lenient load in the production entry.
- `yi-agent-rs/crates/yi-agent-app-server/Cargo.toml` — add `wiremock = "0.6"` under `[dev-dependencies]`.
- `yi-agent-rs/crates/yi-agent/src/main.rs` — TUI first-run Q&A; `run` guidance + non-zero exit; lenient load for app-server.
- `desktop/src/App.tsx` — query `onboarding/status` after handshake; render the wizard; refresh after.
- `desktop/src-tauri/src/lib.rs` + `bridge.rs`? — no; reuse the existing sidecar restart (see Task 11; the Tauri command already exists as `set_relay_url`-style restart — verify before adding anything).
- `docs/project-management/desktop.md`, `yi-agent-cli.md`, `yi-agent-app-server.md`, `README.md` — registration (Task 12).

**Decomposition note:** the runtime module is the shared foundation; app-server, CLI, and desktop each consume it and can be reviewed independently. Tasks are ordered so each depends only on earlier ones.

---

## Task 1: Runtime readiness assessment (pure logic)

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-runtime/src/onboarding.rs`
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/lib.rs`
- Test: in-module `#[cfg(test)] mod tests` in `onboarding.rs`

**Interfaces:**
- Consumes: `crate::models::{ModelCatalog, effective_entry}` (existing).
- Produces:
  - `pub struct EnvModelFields { pub provider: String, pub model: String, pub api_url: String, pub api_key: String }`
  - `pub enum Missing { Provider, Model, ApiUrl, ApiKey }`
  - `pub enum ReadySource { Catalog, Env }`
  - `pub enum Assessment { Ready { source: ReadySource }, Needed { reasons: Vec<Missing> } }`
  - `pub fn is_valid_api_url(url: &str) -> bool`
  - `pub fn assess(env: &EnvModelFields, catalog: Option<&ModelCatalog>) -> Assessment`

- [ ] **Step 1: Write the failing tests**

Create `yi-agent-rs/crates/yi-agent-runtime/src/onboarding.rs` with only the test module and the declarations it references (so it fails to compile until Step 3):

```rust
//! 首次安装引导的共享逻辑：就绪判定、全局 `.env` 写入、连接测试、结束标记。
//!
//! CLI（TUI 首启问答）与 app-server（桌面向导）共用本模块，避免两处各写一份
//! 判定与写入。模型必需字段的落点统一是全局 `~/.yi-agent/.env`。

use crate::models::{effective_entry, ModelCatalog};

/// 从 `.env` 读出的模型相关字段（空串表示未设置）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvModelFields {
    pub provider: String,
    pub model: String,
    pub api_url: String,
    pub api_key: String,
}

/// 导致「未就绪」的缺失项，用于向导首屏说清到底缺什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Missing {
    Provider,
    Model,
    ApiUrl,
    ApiKey,
}

/// 就绪时，配置来自权威层（清单条目）还是兜底层（`.env`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadySource {
    Catalog,
    Env,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Assessment {
    Ready { source: ReadySource },
    Needed { reasons: Vec<Missing> },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{ModelEntry, ModelProvider};

    fn env(provider: &str, model: &str, api_url: &str, api_key: &str) -> EnvModelFields {
        EnvModelFields {
            provider: provider.into(),
            model: model.into(),
            api_url: api_url.into(),
            api_key: api_key.into(),
        }
    }

    fn catalog_with_default(name: &str, api_key: &str) -> ModelCatalog {
        ModelCatalog {
            models: vec![ModelEntry {
                name: name.into(),
                provider: ModelProvider::Anthropic,
                api_url: "https://a".into(),
                model: "m".into(),
                api_key: api_key.into(),
            }],
            default_model: Some(name.into()),
            subagent_model: None,
        }
    }

    #[test]
    fn env_complete_is_ready_from_env() {
        let a = assess(
            &env("openai", "gpt-4o", "https://api.openai.com", "sk-x"),
            None,
        );
        assert_eq!(a, Assessment::Ready { source: ReadySource::Env });
    }

    #[test]
    fn env_without_key_is_needed_listing_the_key() {
        let a = assess(&env("openai", "gpt-4o", "https://api.openai.com", ""), None);
        assert_eq!(a, Assessment::Needed { reasons: vec![Missing::ApiKey] });
    }

    #[test]
    fn env_with_empty_defaults_lists_every_missing_field() {
        let a = assess(&env("", "", "", ""), None);
        match a {
            Assessment::Needed { reasons } => {
                assert!(reasons.contains(&Missing::Provider));
                assert!(reasons.contains(&Missing::Model));
                assert!(reasons.contains(&Missing::ApiKey));
            }
            other => panic!("expected Needed, got {other:?}"),
        }
    }

    #[test]
    fn a_malformed_api_url_is_reported() {
        let a = assess(
            &env("openai", "gpt-4o", "not-a-url", "sk-x"),
            None,
        );
        assert_eq!(a, Assessment::Needed { reasons: vec![Missing::ApiUrl] });
    }

    #[test]
    fn an_empty_api_url_is_allowed() {
        let a = assess(&env("openai", "gpt-4o", "", "sk-x"), None);
        assert_eq!(a, Assessment::Ready { source: ReadySource::Env });
    }

    #[test]
    fn a_catalog_entry_with_a_key_wins_over_env() {
        let cat = catalog_with_default("A", "sk-cat");
        let a = assess(&env("", "", "", ""), Some(&cat));
        assert_eq!(a, Assessment::Ready { source: ReadySource::Catalog });
    }

    #[test]
    fn a_catalog_entry_without_a_key_falls_through_to_env() {
        let cat = catalog_with_default("A", "");
        let a = assess(
            &env("openai", "gpt-4o", "", "sk-x"),
            Some(&cat),
        );
        assert_eq!(a, Assessment::Ready { source: ReadySource::Env });
    }

    #[test]
    fn catalog_none_is_the_cli_path_and_ignores_the_catalog() {
        // CLI 永远传 None，只按 .env 判定。
        let a = assess(&env("anthropic", "claude-x", "", "sk-x"), None);
        assert_eq!(a, Assessment::Ready { source: ReadySource::Env });
    }

    #[test]
    fn api_url_must_be_http_or_https() {
        assert!(is_valid_api_url("https://api.anthropic.com"));
        assert!(is_valid_api_url("http://localhost:8080"));
        assert!(!is_valid_api_url("ftp://x"));
        assert!(!is_valid_api_url("api.anthropic.com"));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime --lib onboarding`
Expected: FAIL — `cannot find function assess` / module not found.

- [ ] **Step 3: Implement `is_valid_api_url` and `assess`**

Add above the `#[cfg(test)]` block in `onboarding.rs`:

```rust
/// `api_url` 必须是以 `http://` 或 `https://` 开头的绝对地址；空串由调用方
/// 当作「用 provider 默认地址」处理，不走本函数。
pub fn is_valid_api_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// 判定当前配置是否足以跑起一次对话。
///
/// 权威层优先：清单（若给了）能解析出默认条目、且该条目 key/model 非空 →
/// `Ready{Catalog}`。否则看兜底层 `.env`：provider 合法、model 非空、api_key
/// 非空、api_url（若给定）格式合法 → `Ready{Env}`；任一不满足 → `Needed` 并
/// 逐个列出原因。CLI 传 `catalog: None`，与「CLI 不读清单」一致。
pub fn assess(env: &EnvModelFields, catalog: Option<&ModelCatalog>) -> Assessment {
    if let Some(cat) = catalog {
        if let Some(entry) = effective_entry(cat, None) {
            if !entry.api_key.trim().is_empty() && !entry.model.trim().is_empty() {
                return Assessment::Ready {
                    source: ReadySource::Catalog,
                };
            }
        }
    }

    let mut reasons = Vec::new();
    if crate::models::ModelProvider::parse(&env.provider).is_none() {
        reasons.push(Missing::Provider);
    }
    if env.model.trim().is_empty() {
        reasons.push(Missing::Model);
    }
    if env.api_key.trim().is_empty() {
        reasons.push(Missing::ApiKey);
    }
    if !env.api_url.trim().is_empty() && !is_valid_api_url(env.api_url.trim()) {
        reasons.push(Missing::ApiUrl);
    }

    if reasons.is_empty() {
        Assessment::Ready {
            source: ReadySource::Env,
        }
    } else {
        Assessment::Needed { reasons }
    }
}
```

- [ ] **Step 4: Register the module**

In `yi-agent-rs/crates/yi-agent-runtime/src/lib.rs`, add after `pub mod models;`:

```rust
pub mod onboarding;
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime --lib onboarding`
Expected: PASS (9 tests).

- [ ] **Step 6: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-runtime/src/onboarding.rs yi-agent-rs/crates/yi-agent-runtime/src/lib.rs
git commit -m "feat(runtime): assess whether a model config is ready"
```

---

## Task 2: Runtime `.env` reader and line-preserving writer

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/onboarding.rs`
- Test: same module's `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `EnvModelFields` (Task 1).
- Produces:
  - `pub const PROVIDER_KEY: &str = "YI_AGENT_PROVIDER";`
  - `pub const MODEL_KEY: &str = "YI_AGENT_MODEL";`
  - `pub const API_URL_KEY: &str = "MODEL_API_URL";`
  - `pub const API_KEY_KEY: &str = "MODEL_API_KEY";`
  - `pub struct ModelSettings { pub provider: String, pub model: String, pub api_url: String, pub api_key: String }`
  - `pub fn read_env_fields(path: &Path) -> std::io::Result<EnvModelFields>`
  - `pub fn write_model_settings(path: &Path, settings: &ModelSettings) -> std::io::Result<()>`
  - `pub enum SettingsError { InvalidProvider(String), EmptyField(&'static str), InvalidApiUrl(String) }`
  - `pub fn validate_settings(settings: &ModelSettings) -> Result<(), SettingsError>`

- [ ] **Step 1: Write the failing tests**

Append to the existing `mod tests` in `onboarding.rs`:

```rust
    use std::path::PathBuf;

    fn settings(provider: &str, model: &str, api_url: &str, api_key: &str) -> ModelSettings {
        ModelSettings {
            provider: provider.into(),
            model: model.into(),
            api_url: api_url.into(),
            api_key: api_key.into(),
        }
    }

    #[test]
    fn read_env_fields_picks_the_four_model_keys() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(
            &path,
            "YI_AGENT_PROVIDER=openai\nYI_AGENT_MODEL=gpt-4o\nMODEL_API_URL=https://api.openai.com\nMODEL_API_KEY=sk-x\nBOCHA_API_KEY=keep\n",
        )
        .unwrap();
        let f = read_env_fields(&path).unwrap();
        assert_eq!(f.provider, "openai");
        assert_eq!(f.model, "gpt-4o");
        assert_eq!(f.api_url, "https://api.openai.com");
        assert_eq!(f.api_key, "sk-x");
    }

    #[test]
    fn read_env_fields_on_a_missing_file_is_all_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let f = read_env_fields(&dir.path().join("nope.env")).unwrap();
        assert_eq!(f, EnvModelFields::default());
    }

    #[test]
    fn write_creates_the_file_with_the_four_keys_and_a_group_comment() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(".env");
        write_model_settings(&path, &settings("openai", "gpt-4o", "https://u", "sk-k")).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("YI_AGENT_PROVIDER=openai"));
        assert!(text.contains("YI_AGENT_MODEL=gpt-4o"));
        assert!(text.contains("MODEL_API_URL=https://u"));
        assert!(text.contains("MODEL_API_KEY=sk-k"));
        assert!(text.contains("# === Model Provider ==="));
        let f = read_env_fields(&path).unwrap();
        assert_eq!(f.api_key, "sk-k");
    }

    #[test]
    fn write_updates_existing_keys_in_place() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(
            &path,
            "# my notes\nYI_AGENT_PROVIDER=anthropic\nYI_AGENT_MAX_TURNS=500\n",
        )
        .unwrap();
        write_model_settings(
            &path,
            &settings("openai", "gpt-4o", "https://u", "sk-k"),
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("YI_AGENT_PROVIDER=openai"));
        assert!(text.contains("YI_AGENT_MAX_TURNS=500"), "unrelated key must survive");
        assert!(text.contains("# my notes"), "comments must survive");
    }

    #[test]
    fn write_appends_missing_keys_and_preserves_user_keys() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(&path, "MY_CUSTOM_KEY=abc\n").unwrap();
        write_model_settings(
            &path,
            &settings("anthropic", "claude-x", "", "sk-k"),
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("MY_CUSTOM_KEY=abc"));
        assert!(text.contains("YI_AGENT_PROVIDER=anthropic"));
        assert!(text.contains("YI_AGENT_MODEL=claude-x"));
        assert!(text.contains("MODEL_API_KEY=sk-k"));
    }

    #[test]
    fn write_does_not_duplicate_an_existing_key() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(&path, "YI_AGENT_MODEL=old\n").unwrap();
        write_model_settings(&path, &settings("openai", "new", "", "sk-k")).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("YI_AGENT_MODEL=").count(), 1);
        assert!(text.contains("YI_AGENT_MODEL=new"));
    }

    #[test]
    fn validate_rejects_a_bad_provider_and_a_bad_url() {
        assert!(matches!(
            validate_settings(&settings("gemini", "m", "", "k")),
            Err(SettingsError::InvalidProvider(_))
        ));
        assert!(matches!(
            validate_settings(&settings("openai", "m", "ftp://x", "k")),
            Err(SettingsError::InvalidApiUrl(_))
        ));
        assert!(matches!(
            validate_settings(&settings("openai", "  ", "", "k")),
            Err(SettingsError::EmptyField("model"))
        ));
        assert!(validate_settings(&settings("openai", "m", "", "k")).is_ok());
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime --lib onboarding`
Expected: FAIL — `cannot find function read_env_fields` etc.

- [ ] **Step 3: Implement reader, writer, validator**

Add above the `#[cfg(test)]` block in `onboarding.rs`:

```rust
use std::path::Path;

/// 全局 `.env` 里模型相关的四个键。
pub const PROVIDER_KEY: &str = "YI_AGENT_PROVIDER";
pub const MODEL_KEY: &str = "YI_AGENT_MODEL";
pub const API_URL_KEY: &str = "MODEL_API_URL";
pub const API_KEY_KEY: &str = "MODEL_API_KEY";

/// 分组注释：缺失键追加时补上，与 `.env.example` / `yi-agent-web` 的格式一致。
const GROUP_COMMENT: &str = "# === Model Provider ===";

/// 引导收敛出的模型配置（写入 `.env` 的四个键）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSettings {
    pub provider: String,
    pub model: String,
    pub api_url: String,
    pub api_key: String,
}

/// 写入前的校验失败。`EmptyField` 携带字段名（`"provider"` / `"model"` / `"api_key"`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsError {
    InvalidProvider(String),
    EmptyField(&'static str),
    InvalidApiUrl(String),
}

impl std::fmt::Display for SettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SettingsError::InvalidProvider(p) => {
                write!(f, "unknown provider: {p}; expected anthropic or openai")
            }
            SettingsError::EmptyField(name) => write!(f, "{name} must not be empty"),
            SettingsError::InvalidApiUrl(u) => {
                write!(f, "api_url must be an absolute http(s) URL: {u}")
            }
        }
    }
}

/// 校验四项是否可写入。`api_url` 允许为空（= 用 provider 默认地址）。
pub fn validate_settings(settings: &ModelSettings) -> Result<(), SettingsError> {
    if crate::models::ModelProvider::parse(&settings.provider).is_none() {
        return Err(SettingsError::InvalidProvider(settings.provider.clone()));
    }
    if settings.model.trim().is_empty() {
        return Err(SettingsError::EmptyField("model"));
    }
    if settings.api_key.trim().is_empty() {
        return Err(SettingsError::EmptyField("api_key"));
    }
    if !settings.api_url.trim().is_empty() && !is_valid_api_url(settings.api_url.trim()) {
        return Err(SettingsError::InvalidApiUrl(settings.api_url.clone()));
    }
    Ok(())
}

/// 从 `.env` 读四个模型键；文件不存在 → 全空（与 `read` 的既有约定一致）。
pub fn read_env_fields(path: &Path) -> std::io::Result<EnvModelFields> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(EnvModelFields::default())
        }
        Err(error) => return Err(error),
    };
    let mut fields = EnvModelFields::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(eq) = line.find('=') else { continue };
        let key = line[..eq].trim();
        let value = strip_quotes(line[eq + 1..].trim());
        match key {
            PROVIDER_KEY => fields.provider = value,
            MODEL_KEY => fields.model = value,
            API_URL_KEY => fields.api_url = value,
            API_KEY_KEY => fields.api_key = value,
            _ => {}
        }
    }
    Ok(fields)
}

fn strip_quotes(s: &str) -> String {
    if s.len() >= 2 {
        let b = s.as_bytes();
        if (b[0] == b'"' && b[b.len() - 1] == b'"') || (b[0] == b'\'' && b[b.len() - 1] == b'\'') {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

/// 行级保留式写入：只就地替换/追加这四个键，其余行（含用户自定义键与注释）
/// 逐字节保留。落盘用「临时文件 + rename」保证原子性，与 `models.json` /
/// `preferences.json` 同一约定。文件含密钥，新建时权限收紧为 `0600`。
pub fn write_model_settings(path: &Path, settings: &ModelSettings) -> std::io::Result<()> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut lines: Vec<String> = existing.lines().map(str::to_string).collect();

    let mut replaced = [false; 4];
    let targets = [
        (PROVIDER_KEY, settings.provider.as_str()),
        (MODEL_KEY, settings.model.as_str()),
        (API_URL_KEY, settings.api_url.as_str()),
        (API_KEY_KEY, settings.api_key.as_str()),
    ];

    for line in lines.iter_mut() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some(eq) = trimmed.find('=') else { continue };
        let key = trimmed[..eq].trim();
        for (i, (name, value)) in targets.iter().enumerate() {
            if key == *name {
                *line = format!("{name}={value}");
                replaced[i] = true;
            }
        }
    }

    let missing: Vec<&(&str, &str)> = targets
        .iter()
        .enumerate()
        .filter(|(i, _)| !replaced[*i])
        .map(|(_, t)| t)
        .collect();
    if !missing.is_empty() {
        lines.push(GROUP_COMMENT.to_string());
        for (name, value) in missing {
            lines.push(format!("{name}={value}"));
        }
    }

    let mut output = lines.join("\n");
    output.push('\n');

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp-onboarding-{}", std::process::id()));
    std::fs::write(&tmp, output)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // 新文件收紧到 0600；若目标已存在，rename 前把临时文件的权限对齐它，
        // 避免把用户放宽过的权限悄悄改回 0600。
        let mode = std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o777)
            .unwrap_or(0o600);
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode));
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime --lib onboarding`
Expected: PASS (all Task 1 + Task 2 tests).

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-runtime/src/onboarding.rs
git commit -m "feat(runtime): line-preserving global .env writer for onboarding"
```

---

## Task 3: Runtime "onboarding finished" marker

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/onboarding.rs`
- Test: same module's `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: nothing new.
- Produces:
  - `pub const DISMISSED_KEY: &str = "onboarding_dismissed";`
  - `pub fn load_dismissed(preferences_path: &Path) -> bool`
  - `pub fn save_dismissed(preferences_path: &Path, value: bool) -> std::io::Result<()>`

- [ ] **Step 1: Write the failing tests**

Append to `mod tests`:

```rust
    #[test]
    fn missing_preferences_means_not_dismissed() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(!load_dismissed(&dir.path().join("preferences.json")));
    }

    #[test]
    fn a_corrupt_preferences_file_reads_as_not_dismissed() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("preferences.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert!(!load_dismissed(&path));
    }

    #[test]
    fn save_then_load_round_trips_and_preserves_other_keys() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("preferences.json");
        std::fs::write(&path, r#"{"theme":"light"}"#).unwrap();
        save_dismissed(&path, true).unwrap();
        assert!(load_dismissed(&path));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"theme\""), "other keys must survive: {text}");
        save_dismissed(&path, false).unwrap();
        assert!(!load_dismissed(&path));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime --lib onboarding`
Expected: FAIL — `cannot find function load_dismissed`.

- [ ] **Step 3: Implement the marker**

Add to `onboarding.rs` (it already depends on `serde_json` via the crate's deps):

```rust
/// `preferences.json` 里「引导已结束」的键：完成或用户选「稍后设置」都置位。
pub const DISMISSED_KEY: &str = "onboarding_dismissed";

/// 读结束标记。缺文件 / 不可读 / 损坏一律回 `false`（照常可能弹引导），
/// 坏偏好绝不阻断启动——与 `settings_store` 的既有约定一致。
pub fn load_dismissed(preferences_path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(preferences_path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    value
        .get(DISMISSED_KEY)
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// 写结束标记：读-改-写并保留其余键，落盘用「临时文件 + rename」保证原子性。
pub fn save_dismissed(preferences_path: &Path, value: bool) -> std::io::Result<()> {
    let existing = std::fs::read_to_string(preferences_path).unwrap_or_default();
    let mut object = serde_json::from_str::<serde_json::Value>(&existing)
        .ok()
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    object.insert(DISMISSED_KEY.to_string(), serde_json::Value::Bool(value));

    if let Some(parent) = preferences_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = preferences_path.with_extension(format!("tmp-onboarding-{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec_pretty(&object)?)?;
    std::fs::rename(&tmp, preferences_path)?;
    Ok(())
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime --lib onboarding`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-runtime/src/onboarding.rs
git commit -m "feat(runtime): persist the onboarding-finished marker"
```

---

## Task 4: Runtime connection test

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/onboarding.rs`
- Modify: `yi-agent-rs/crates/yi-agent-runtime/Cargo.toml`
- Test: same module's `#[cfg(test)] mod tests` (wiremock)

**Interfaces:**
- Consumes: `ModelSettings` (Task 2); `yi_agent_llm::{AnthropicProvider, AnthropicProviderOpts, OpenaiProvider, OpenaiProviderOpts}`; `yi_agent_core::{Provider, ProviderRequest, GenParams, Message}`; `ProviderError`.
- Produces:
  - `pub struct ConnectionOutcome { pub ok: bool, pub reason: Option<String> }`
  - `pub async fn test_connection(settings: &ModelSettings) -> ConnectionOutcome`

- [ ] **Step 1: Add the dev-dependency**

In `yi-agent-rs/crates/yi-agent-runtime/Cargo.toml`, under `[dev-dependencies]` (next to `tempfile = "3"`), add exactly:

```toml
wiremock = "0.6"
```

- [ ] **Step 2: Write the failing tests**

Append to `mod tests` in `onboarding.rs`:

```rust
    use wiremock::matchers::{method as wm_method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn anthropic_ping_body() -> serde_json::Value {
        serde_json::json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "ok"}],
            "model": "m",
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })
    }

    #[tokio::test]
    async fn a_working_endpoint_reports_ok() {
        let server = MockServer::start().await;
        Mock::given(wm_method("POST"))
            .and(wm_path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_ping_body()))
            .mount(&server)
            .await;
        let outcome = test_connection(&settings("anthropic", "m", &server.uri(), "sk-x")).await;
        assert_eq!(outcome.ok, true, "reason: {:?}", outcome.reason);
    }

    #[tokio::test]
    async fn a_401_reads_as_an_invalid_key() {
        let server = MockServer::start().await;
        Mock::given(wm_method("POST"))
            .and(wm_path("/v1/messages"))
            .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
            .mount(&server)
            .await;
        let outcome = test_connection(&settings("anthropic", "m", &server.uri(), "bad")).await;
        assert!(!outcome.ok);
        assert_eq!(outcome.reason.as_deref(), Some("API 密钥无效或无权限"));
    }

    #[tokio::test]
    async fn an_unreachable_endpoint_reads_as_cannot_connect() {
        // 端口 1 上必然连不上。
        let outcome =
            test_connection(&settings("anthropic", "m", "http://127.0.0.1:1", "sk-x")).await;
        assert!(!outcome.ok);
        assert_eq!(outcome.reason.as_deref(), Some("无法连接 API 地址"));
    }

    #[tokio::test]
    async fn a_400_reads_as_a_rejected_model() {
        let server = MockServer::start().await;
        Mock::given(wm_method("POST"))
            .and(wm_path("/v1/messages"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad model"))
            .mount(&server)
            .await;
        let outcome = test_connection(&settings("anthropic", "nope", &server.uri(), "sk-x")).await;
        assert!(!outcome.ok);
        assert_eq!(outcome.reason.as_deref(), Some("模型标识不被该地址接受"));
    }
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime --lib onboarding`
Expected: FAIL — `cannot find function test_connection`.

- [ ] **Step 4: Implement the connection test**

Add to `onboarding.rs`:

```rust
/// 一次连接测试的结论。`reason` 是给人看的中文；成功时为 `None`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionOutcome {
    pub ok: bool,
    pub reason: Option<String>,
}

/// 用一份临时 provider 发一次最小请求，验证 key/url/model 是否可用。
///
/// **只探测，不写任何文件**。明文 key 只在此处进请求头，绝不进日志或返回值。
pub async fn test_connection(settings: &ModelSettings) -> ConnectionOutcome {
    use yi_agent_core::{GenParams, Message, Provider, ProviderRequest};

    let provider: std::sync::Arc<dyn Provider> = match settings.provider.as_str() {
        "openai" => match yi_agent_llm::OpenaiProvider::new(yi_agent_llm::OpenaiProviderOpts {
            base_url: Some(settings.api_url.clone()),
            api_key: Some(settings.api_key.clone()),
            ..Default::default()
        }) {
            Ok(p) => std::sync::Arc::new(p),
            Err(error) => return failure(error),
        },
        _ => match yi_agent_llm::AnthropicProvider::new(yi_agent_llm::AnthropicProviderOpts {
            base_url: Some(settings.api_url.clone()),
            api_key: Some(settings.api_key.clone()),
            ..Default::default()
        }) {
            Ok(p) => std::sync::Arc::new(p),
            Err(error) => return failure(error),
        },
    };

    let request = ProviderRequest {
        model: settings.model.clone(),
        system: None,
        messages: vec![Message::user("ping")],
        tools: Vec::new(),
        params: GenParams {
            max_tokens: Some(1),
            ..Default::default()
        },
    };

    match provider.call(request).await {
        Ok(_) => ConnectionOutcome {
            ok: true,
            reason: None,
        },
        Err(error) => failure(error),
    }
}

/// 把 provider 错误翻成一句可读中文。明文响应体只用于「其它」分支的粗粒度提示，
/// 且经过截断——绝不外泄给界面之外。
fn failure(error: yi_agent_core::ProviderError) -> ConnectionOutcome {
    use yi_agent_core::ProviderError;
    let reason = match error {
        ProviderError::Auth(_) => "API 密钥无效或无权限".to_string(),
        ProviderError::InvalidRequest(_) => "模型标识不被该地址接受".to_string(),
        ProviderError::RateLimited => "请求过于频繁，请稍后重试".to_string(),
        ProviderError::Network(message) => {
            let lower = message.to_ascii_lowercase();
            if lower.contains("timed out") || lower.contains("timeout") {
                "连接超时".to_string()
            } else {
                "无法连接 API 地址".to_string()
            }
        }
        ProviderError::Server(message) | ProviderError::Stream(message) => {
            format!("服务端错误：{}", truncate(&message, 120))
        }
    };
    ConnectionOutcome {
        ok: false,
        reason: Some(reason),
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max).collect::<String>() + "…"
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime --lib onboarding`
Expected: PASS. If the anthropic ping needs a different path, check `yi-agent-rs/crates/yi-agent-llm/src/anthropic/client.rs` for the exact endpoint and adjust `wm_path` — the request path in the production client is authoritative.

- [ ] **Step 6: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-runtime/src/onboarding.rs yi-agent-rs/crates/yi-agent-runtime/Cargo.toml
git commit -m "feat(runtime): probe a model config with a minimal request"
```

---

## Task 5: Runtime lenient config load (key-less startup)

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/config.rs`
- Test: `mod tests` in `config.rs`

**Context / why:** `RuntimeConfig::load` bails with `API key required` when no key is present. The desktop sidecar must start on a fresh machine so it can serve `onboarding/*`; otherwise the wizard is unreachable. `load_lenient` is the same loader with an empty key allowed.

**Interfaces:**
- Consumes: `ConfigOverrides`, `RuntimeConfig` (existing).
- Produces: `impl RuntimeConfig { pub fn load_lenient(overrides: &ConfigOverrides) -> Result<Self> }`

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `config.rs` (near the other `RuntimeConfig::load` tests). Use the existing `isolated_config_env()` helper and `ENV_TEST_MUTEX` already in that module:

```rust
    #[test]
    fn load_lenient_allows_a_missing_api_key() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _env = isolated_config_env();
        let temp = tempfile::TempDir::new().expect("tempdir");
        let overrides = ConfigOverrides {
            workdir: Some(temp.path().to_path_buf()),
            ..ConfigOverrides::default()
        };
        let config = RuntimeConfig::load_lenient(&overrides).expect("lenient load must succeed");
        assert_eq!(config.api_key, "");
    }

    #[test]
    fn load_still_requires_a_key() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _env = isolated_config_env();
        let temp = tempfile::TempDir::new().expect("tempdir");
        let overrides = ConfigOverrides {
            workdir: Some(temp.path().to_path_buf()),
            ..ConfigOverrides::default()
        };
        assert!(RuntimeConfig::load(&overrides).is_err());
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime --lib load_lenient`
Expected: FAIL — `no function load_lenient`.

- [ ] **Step 3: Implement it**

Refactor `RuntimeConfig::load` into a shared core and two thin wrappers. In `config.rs`, change the `pub fn load` body so the key handling is parameterised. Replace the existing `pub fn load(...)` definition with:

```rust
    /// 从覆盖项 + 环境变量加载配置。
    ///
    /// 优先级:覆盖项 > 环境变量 > 默认值。
    /// .env 加载:显式指定 workdir 时只加载指定目录,fallback 模式合并全局兜底。
    /// fallback 模式下只读取已存在的 `.yi-agent/.env`,不主动创建目录。
    pub fn load(overrides: &ConfigOverrides) -> Result<Self> {
        Self::load_inner(overrides, false)
    }

    /// 与 [`load`] 相同，但**允许 API key 缺失**（回空串）。
    ///
    /// 桌面侧车在新机器上必须能起来才能提供引导 RPC；缺 key 时按空 key 加载，
    /// 由调用方先跑引导再真正发起对话。
    pub fn load_lenient(overrides: &ConfigOverrides) -> Result<Self> {
        Self::load_inner(overrides, true)
    }

    fn load_inner(overrides: &ConfigOverrides, allow_missing_key: bool) -> Result<Self> {
        // 这里原样保留改动前 `load` 的**全部**函数体（从 `let local_env_path = ...`
        // 到最后的 `Ok(Self { ... })`），只把下面那个 `api_key` 块按第 3 步替换。
        // 不要重写函数体，只做两处改动：换签名、换 api_key 块。
    }
```

Then, inside `load_inner`, replace the strict key block:

```rust
        let api_key = overrides
            .api_key
            .clone()
            .or_else(|| std::env::var("MODEL_API_KEY").ok())
            .context("API key required: set MODEL_API_KEY or use --api-key")?;
        if api_key.is_empty() {
            bail!("API key is empty: set MODEL_API_KEY or use --api-key");
        }
```

with:

```rust
        let api_key = match overrides
            .api_key
            .clone()
            .or_else(|| std::env::var("MODEL_API_KEY").ok())
        {
            Some(key) if !key.is_empty() => key,
            _ if allow_missing_key => String::new(),
            Some(_) => bail!("API key is empty: set MODEL_API_KEY or use --api-key"),
            None => bail!(
                "API key required: set MODEL_API_KEY or use --api-key"
            ),
        };
```

Everything else in the original body stays byte-for-byte identical.

**Import fix (clippy `-D warnings` will fail otherwise):** the block you just removed held the file's **only** `.context(...)` call, so `Context` becomes unused. In `config.rs`, change line 9:

```rust
use anyhow::{Context, Result, bail};
```

to:

```rust
use anyhow::{Result, bail};
```

(Verify first with `grep -n "\.context(\|with_context(" yi-agent-rs/crates/yi-agent-runtime/src/config.rs` — it should report zero matches after the replacement. If any other call exists, keep `Context`.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime --lib config`
Expected: PASS, including all pre-existing config tests (the strict path must be unchanged).

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-runtime/src/config.rs
git commit -m "feat(runtime): add a lenient config load for key-less startup"
```

---

## Task 6: app-server `onboarding/*` RPC

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/onboarding_rpc.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/Cargo.toml`
- Test: `mod tests` in `onboarding_rpc.rs`

**Interfaces:**
- Consumes: everything from Tasks 1–5, plus `crate::model_rpc`'s import logic is NOT reused directly — `onboarding/apply` calls `yi_agent_runtime::models` to land the catalog entry (mirroring `model_rpc::import_env`), and `crate::protocol::RpcError`.
- Produces:
  - `pub fn handle_onboarding_request_at(env_path: &Path, preferences_path: &Path, models_path: &Path, method: &str, params: &Value, fallback: &RuntimeConfig) -> Result<Value, RpcError>`
  - `pub fn settings_from_params(params: &Value) -> Result<yi_agent_runtime::onboarding::ModelSettings, RpcError>`

  Methods handled here: `onboarding/status`, `onboarding/apply`, `onboarding/dismiss`.
  **`onboarding/test` is NOT here** — it needs an `await`, so `server.rs` intercepts it and calls `settings_from_params` + `yi_agent_runtime::onboarding::test_connection` directly (Task 6b). Keeping the sync handler free of `test` avoids translate-then-reparse.

- [ ] **Step 1: Write the failing tests**

Create `yi-agent-rs/crates/yi-agent-app-server/src/onboarding_rpc.rs` with the test module first:

```rust
//! `onboarding/*` RPC：首次安装引导。路径可注入，测试不碰真实 HOME。
//!
//! 与 `model_rpc.rs` 同类：把 `method` + `params` 翻成对 runtime 引导模块的调用。

use crate::protocol::RpcError;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use yi_agent_runtime::config::RuntimeConfig;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Paths {
        _dir: tempfile::TempDir,
        env: PathBuf,
        prefs: PathBuf,
        models: PathBuf,
    }

    fn paths() -> Paths {
        let dir = tempfile::TempDir::new().unwrap();
        let env = dir.path().join(".yi-agent").join(".env");
        let prefs = dir.path().join(".yi-agent").join("preferences.json");
        let models = dir.path().join(".yi-agent").join("models.json");
        Paths { _dir: dir, env, prefs, models }
    }

    fn fallback() -> RuntimeConfig {
        RuntimeConfig {
            provider: "anthropic".to_string(),
            api_url: "https://api.anthropic.com".to_string(),
            api_key: String::new(),
            model: "cfg-model".to_string(),
            max_turns: 20,
            max_resident_subagents: yi_agent_runtime::config::RESIDENT_SUBAGENTS_DEFAULT,
            max_direct_children: yi_agent_runtime::config::DIRECT_CHILDREN_DEFAULT,
            workdir: PathBuf::from("/tmp/onboarding-rpc-test"),
            system_prompt: None,
            compact_threshold: 160_000,
            compact_user_budget_tokens: 20_000,
            compact_tool_budget_tokens: 12_000,
            yolo: false,
            sandbox_promotable: true,
            sandbox: yi_agent_tools::SandboxMode::WorkspaceWrite,
            sandbox_writable_roots: Vec::new(),
            skills_catalog_budget: 8192,
            skills_catalog_budget_explicit: false,
        }
    }

    fn call(p: &Paths, method: &str, params: Value) -> Result<Value, RpcError> {
        handle_onboarding_request_at(&p.env, &p.prefs, &p.models, method, &params, &fallback())
    }

    #[test]
    fn status_reports_needed_with_reasons_on_a_blank_machine() {
        let p = paths();
        let out = call(&p, "onboarding/status", json!({})).unwrap();
        assert_eq!(out["needed"], true);
        assert_eq!(out["dismissed"], false);
        let reasons = out["reasons"].as_array().unwrap();
        assert!(reasons.iter().any(|r| r == "api_key"), "got {reasons:?}");
    }

    #[test]
    fn status_reports_ready_when_the_env_is_complete() {
        let p = paths();
        call(
            &p,
            "onboarding/apply",
            json!({"provider":"openai","model":"gpt-4o","api_url":"","api_key":"sk-x"}),
        )
        .unwrap();
        let out = call(&p, "onboarding/status", json!({})).unwrap();
        assert_eq!(out["needed"], false);
    }

    #[test]
    fn apply_writes_the_env_and_lands_the_catalog_entry() {
        let p = paths();
        let out = call(
            &p,
            "onboarding/apply",
            json!({"provider":"openai","model":"gpt-4o","api_url":"https://u","api_key":"sk-x"}),
        )
        .unwrap();
        assert_eq!(out["ok"], true);
        assert_eq!(out["env_written"], true);
        assert_eq!(out["imported"], true);

        let text = std::fs::read_to_string(&p.env).unwrap();
        assert!(text.contains("YI_AGENT_PROVIDER=openai"));
        assert!(text.contains("MODEL_API_KEY=sk-x"));

        let catalog = yi_agent_runtime::models::load_catalog_from(&p.models);
        assert_eq!(catalog.default_model.as_deref(), Some("gpt-4o"));
        assert_eq!(catalog.models[0].api_key, "sk-x");
    }

    #[test]
    fn apply_with_an_invalid_provider_writes_nothing() {
        let p = paths();
        let err = call(
            &p,
            "onboarding/apply",
            json!({"provider":"gemini","model":"m","api_url":"","api_key":"k"}),
        )
        .unwrap_err();
        assert_eq!(err.data.unwrap()["code"], "invalid_model");
        assert!(!p.env.exists(), "a rejected apply must not write .env");
        assert!(!p.models.exists(), "a rejected apply must not write models.json");
    }

    #[test]
    fn apply_never_returns_the_raw_key() {
        let p = paths();
        let out = call(
            &p,
            "onboarding/apply",
            json!({"provider":"openai","model":"gpt-4o","api_url":"","api_key":"sk-secret-1234"}),
        )
        .unwrap();
        assert!(!out.to_string().contains("sk-secret-1234"), "got {out}");
    }

    #[test]
    fn dismiss_marks_and_loads_back() {
        let p = paths();
        call(&p, "onboarding/dismiss", json!({})).unwrap();
        let out = call(&p, "onboarding/status", json!({})).unwrap();
        assert_eq!(out["dismissed"], true);
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let p = paths();
        let err = call(&p, "onboarding/bogus", json!({})).unwrap_err();
        assert_eq!(err.code, -32601);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib onboarding_rpc`
Expected: FAIL — module not found / function not defined.

- [ ] **Step 3: Implement the handler**

Add above the `#[cfg(test)]` block in `onboarding_rpc.rs`:

```rust
/// `onboarding/*` 的唯一入口。路径与兜底 cfg 均由调用方注入（测试不碰真实 HOME）。
pub fn handle_onboarding_request_at(
    env_path: &Path,
    preferences_path: &Path,
    models_path: &Path,
    method: &str,
    params: &Value,
    fallback: &RuntimeConfig,
) -> Result<Value, RpcError> {
    match method {
        "onboarding/status" => {
            let env = yi_agent_runtime::onboarding::read_env_fields(env_path)
                .map_err(|e| RpcError::internal(e.to_string()))?;
            let catalog = yi_agent_runtime::models::load_catalog_from(models_path);
            let assessment = yi_agent_runtime::onboarding::assess(&env, Some(&catalog));
            let dismissed = yi_agent_runtime::onboarding::load_dismissed(preferences_path);
            let (needed, reasons) = match assessment {
                yi_agent_runtime::onboarding::Assessment::Ready { .. } => (false, Vec::new()),
                yi_agent_runtime::onboarding::Assessment::Needed { reasons } => (
                    true,
                    reasons
                        .into_iter()
                        .map(missing_name)
                        .map(str::to_string)
                        .collect::<Vec<_>>(),
                ),
            };
            Ok(json!({ "needed": needed, "dismissed": dismissed, "reasons": reasons }))
        }
        "onboarding/apply" => apply(env_path, models_path, params, fallback),
        "onboarding/dismiss" => {
            yi_agent_runtime::onboarding::save_dismissed(preferences_path, true)
                .map_err(|e| RpcError::internal(e.to_string()))?;
            Ok(json!({ "ok": true }))
        }
        _ => Err(RpcError::method_not_found(method)),
    }
}

fn missing_name(missing: yi_agent_runtime::onboarding::Missing) -> &'static str {
    use yi_agent_runtime::onboarding::Missing;
    match missing {
        Missing::Provider => "provider",
        Missing::Model => "model",
        Missing::ApiUrl => "api_url",
        Missing::ApiKey => "api_key",
    }
}

/// 把 `params` 翻成 `ModelSettings` 并校验；校验失败 → `invalid_model`、零写入。
/// `server.rs` 的 `onboarding/test` 分支也复用它（本轮不落盘，只探测）。
pub fn settings_from_params(
    params: &Value,
) -> Result<yi_agent_runtime::onboarding::ModelSettings, RpcError> {
    let settings = yi_agent_runtime::onboarding::ModelSettings {
        provider: string_field(params, "provider").unwrap_or_default(),
        model: string_field(params, "model").unwrap_or_default(),
        api_url: string_field(params, "api_url").unwrap_or_default(),
        api_key: string_field(params, "api_key").unwrap_or_default(),
    };
    yi_agent_runtime::onboarding::validate_settings(&settings)
        .map_err(|e| RpcError::invalid_model(e.to_string()))?;
    Ok(settings)
}

fn apply(
    env_path: &Path,
    models_path: &Path,
    params: &Value,
    _fallback: &RuntimeConfig,
) -> Result<Value, RpcError> {
    let settings = settings_from_params(params)?;

    // 1) 写全局 .env（行级保留式、原子）。
    yi_agent_runtime::onboarding::write_model_settings(env_path, &settings)
        .map_err(|e| RpcError::internal(e.to_string()))?;

    // 2) 尝试收边到清单。失败不吞：如实回包 imported=false + 原因。
    let import_result = import_into_catalog(models_path, &settings);
    match import_result {
        Ok(name) => Ok(json!({
            "ok": true,
            "env_written": true,
            "imported": true,
            "name": name,
        })),
        Err(error) => Ok(json!({
            "ok": true,
            "env_written": true,
            "imported": false,
            "import_error": error,
        })),
    }
}

/// 把配置落成清单里的一条并设为默认；重名自动加后缀。语义与
/// `model_rpc::import_env` 一致，但入参是引导收敛出的 settings。
fn import_into_catalog(
    models_path: &Path,
    settings: &yi_agent_runtime::onboarding::ModelSettings,
) -> Result<String, String> {
    use yi_agent_runtime::models::{ModelCatalog, ModelEntry, ModelProvider, save_catalog_to};

    let Some(provider) = ModelProvider::parse(&settings.provider) else {
        return Err("unknown provider".to_string());
    };
    let mut catalog: ModelCatalog = yi_agent_runtime::models::load_catalog_from(models_path);
    let base = settings.model.trim().to_string();
    let mut name = base.clone();
    let mut n = 2u32;
    while catalog.models.iter().any(|m| m.name == name) {
        name = format!("{base}-{n}");
        n += 1;
    }
    catalog.models.push(ModelEntry {
        name: name.clone(),
        provider,
        api_url: settings.api_url.clone(),
        model: settings.model.clone(),
        api_key: settings.api_key.clone(),
    });
    catalog.default_model = Some(name.clone());
    save_catalog_to(models_path, &catalog).map_err(|e| e.to_string())?;
    Ok(name)
}

fn string_field(params: &Value, field: &str) -> Option<String> {
    params
        .get(field)
        .and_then(|v| v.as_str())
        .map(str::to_string)
}
```

Note on `onboarding/test`: real network probing happens in `server.rs` (async), so it is not part of this module (see Task 6b). `settings_from_params` is exported for that branch to reuse.

- [ ] **Step 4: Add the wiremock dev-dependency**

In `yi-agent-rs/crates/yi-agent-app-server/Cargo.toml` under `[dev-dependencies]`:

```toml
wiremock = "0.6"
```

- [ ] **Step 5: Register the module**

In `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs`, add:

```rust
pub mod onboarding_rpc;
```

(Match the existing style — check how `model_rpc` is declared and mirror it, including any `pub use`.)

- [ ] **Step 6: Run the unit tests**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib onboarding_rpc`
Expected: PASS (status / apply / dismiss / unknown-method cases).

- [ ] **Step 7: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/onboarding_rpc.rs yi-agent-rs/crates/yi-agent-app-server/src/lib.rs yi-agent-rs/crates/yi-agent-app-server/Cargo.toml
git commit -m "feat(app-server): onboarding status/apply/test/dismiss RPC"
```

---

## Task 6b: Wire `onboarding/*` into the server dispatch + async test

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`
- Test: `mod tests` in `server.rs`

**Interfaces:**
- Consumes: `crate::onboarding_rpc::{handle_onboarding_request_at, settings_from_params}`.
- Produces: dispatch for the four methods with a `Control` scope gate; `onboarding/test` runs `yi_agent_runtime::onboarding::test_connection` and returns `{ok, reason?}`.

**Context:** add `onboarding_env_path` and `onboarding_preferences_path` to `RuntimeAttachments` (injectable for the same reason `models_path` is). Production sets them to `~/.yi-agent/.env` and `~/.yi-agent/preferences.json`; the test harness sets them into its tempdir.

- [ ] **Step 1: Extend `RuntimeAttachments`**

In `RuntimeAttachments` add:

```rust
    /// Where the global `.env` lives, for `onboarding/apply`. Injectable for the
    /// same reason `models_path` is: onboarding must never write the developer's
    /// real `~/.yi-agent/.env`.
    pub(crate) onboarding_env_path: PathBuf,
    /// Where `preferences.json` lives, for the `onboarding_dismissed` marker.
    pub(crate) onboarding_preferences_path: PathBuf,
```

- [ ] **Step 2: Write the failing dispatch test**

Add to `mod tests` in `server.rs`:

```rust
    #[tokio::test(flavor = "multi_thread")]
    async fn onboarding_status_is_readable_and_reports_needed() {
        let mut h = Harness::new();
        initialize(&mut h).await;
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"onboarding/status","params":{}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 2);
        assert_eq!(v["result"]["needed"], true);
        assert_eq!(v["result"]["dismissed"], false);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn onboarding_apply_lands_the_env_and_catalog() {
        let mut h = Harness::new();
        initialize(&mut h).await;
        h.send(
            r#"{"jsonrpc":"2.0","id":2,"method":"onboarding/apply",
               "params":{"provider":"openai","model":"gpt-4o","api_url":"","api_key":"sk-x"}}"#,
        )
        .await;
        let v = h.read_value().await;
        assert_eq!(v["result"]["env_written"], true);
        assert_eq!(v["result"]["imported"], true);

        // 状态随即翻成 ready。
        h.send(r#"{"jsonrpc":"2.0","id":3,"method":"onboarding/status","params":{}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(v["result"]["needed"], false);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn onboarding_test_probes_the_endpoint() {
        use wiremock::matchers::{method as wm_method, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(wm_method("POST"))
            .and(wm_path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "type":"message","role":"assistant",
                "content":[{"type":"text","text":"ok"}],"model":"m",
                "stop_reason":"end_turn",
                "usage":{"input_tokens":1,"output_tokens":1}
            })))
            .mount(&server)
            .await;

        let mut h = Harness::new();
        initialize(&mut h).await;
        let req = serde_json::json!({
            "jsonrpc":"2.0","id":2,"method":"onboarding/test",
            "params":{"provider":"anthropic","model":"m",
                      "api_url": server.uri(), "api_key":"sk-x"}
        });
        h.send(&req.to_string()).await;
        let v = h.read_value().await;
        assert_eq!(v["result"]["ok"], true, "got {v}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_observe_client_cannot_apply_onboarding() {
        let mut h = Harness::with_scope(Scope::Observe).await;
        initialize(&mut h).await;
        h.send(
            r#"{"jsonrpc":"2.0","id":2,"method":"onboarding/apply",
               "params":{"provider":"openai","model":"m","api_url":"","api_key":"k"}}"#,
        )
        .await;
        let v = h.read_value().await;
        assert_eq!(v["error"]["code"], -32014);
    }
```

- [ ] **Step 3: Run to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib onboarding_`
Expected: FAIL — method not found.

- [ ] **Step 4: Add the dispatch arm**

In `server.rs`, next to the existing `"model/*"` arms, add (mirroring their shape). `onboarding/status` is read-only → `Observe`; the other three need `Control`:

```rust
                    "onboarding/status" => {
                        match crate::onboarding_rpc::handle_onboarding_request_at(
                            &onboarding_env_path,
                            &onboarding_preferences_path,
                            &models_path,
                            method.as_str(),
                            &req.params,
                            &cfg,
                        ) {
                            Ok(value) => {
                                write_response(&hub, &client, ok_response(id, value)).await?
                            }
                            Err(error) => {
                                write_response(&hub, &client, err_response(id, error)).await?
                            }
                        }
                    }
                    "onboarding/apply" | "onboarding/dismiss" => {
                        if client_scope < Scope::Control {
                            write_response(
                                &hub,
                                &client,
                                err_response(id, RpcError::insufficient_scope(Scope::Control)),
                            )
                            .await?;
                            continue;
                        }
                        match crate::onboarding_rpc::handle_onboarding_request_at(
                            &onboarding_env_path,
                            &onboarding_preferences_path,
                            &models_path,
                            method.as_str(),
                            &req.params,
                            &cfg,
                        ) {
                            Ok(value) => {
                                write_response(&hub, &client, ok_response(id, value)).await?
                            }
                            Err(error) => {
                                write_response(&hub, &client, err_response(id, error)).await?
                            }
                        }
                    }
                    "onboarding/test" => {
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
                            match crate::onboarding_rpc::settings_from_params(&req.params) {
                                Ok(s) => s,
                                Err(error) => {
                                    write_response(&hub, &client, err_response(id, error)).await?;
                                    continue;
                                }
                            };
                        let outcome =
                            yi_agent_runtime::onboarding::test_connection(&settings).await;
                        let value = serde_json::json!({
                            "ok": outcome.ok,
                            "reason": outcome.reason,
                        });
                        write_response(&hub, &client, ok_response(id, value)).await?;
                    }
```

Destructure the two new attachment fields in `serve` (where `RuntimeAttachments` is destructured) and thread them into the arms. In `run`/`serve_stdio`, set `onboarding_env_path` to `yi_agent_runtime::config::resolve_global_env_path().unwrap_or_default()` and `onboarding_preferences_path` to `<home>/.yi-agent/preferences.json`.

- [ ] **Step 5: Update the test harness**

In `Harness::with_config_and_scope_and_watchman` (and any other `RuntimeAttachments { .. }` construction in tests at `server.rs:9165/9980/10052/10128/16881`), set the two new fields to tempdir paths, e.g. `index_dir.path().join(".env")` and `index_dir.path().join("preferences.json")`.

- [ ] **Step 6: Run tests to verify they pass**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib onboarding_`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): dispatch onboarding RPCs with scope gates"
```

---

## Task 6c: app-server starts without a key

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs` (`run_app_server`)
- Test: `mod tests` in `main.rs`

**Context:** `run_app_server` calls `config::load(&cli)?`, which fails on a fresh machine. Switch it to `load_lenient` so the sidecar serves `onboarding/*`.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `main.rs`:

```rust
    #[test]
    fn app_server_mode_uses_a_lenient_config_load() {
        // 生产入口缺 key 也必须能起来，否则新机器上引导 RPC 不可达。
        let source = include_str!("main.rs");
        assert!(
            source.contains("config::load_lenient(&cli)"),
            "run_app_server must use the lenient loader"
        );
    }
```

(This is a source-level guard, mirroring the existing `default_system_prompt_...` style of tests in this file. A behavioural test would need to spawn the whole server; the guard plus Task 6b's dispatch tests cover the contract.)

- [ ] **Step 2: Run to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent app_server_mode_uses_a_lenient`
Expected: FAIL.

- [ ] **Step 3: Implement**

In `yi-agent-rs/crates/yi-agent/src/config.rs`, add:

```rust
/// 与 [`load`] 相同，但允许 API key 缺失（桌面侧车首启场景）。
pub fn load_lenient(cli: &Cli) -> Result<Config> {
    yi_agent_runtime::config::RuntimeConfig::load_lenient(&cli.into())
}
```

In `main.rs::run_app_server`, change `let config = config::load(&cli)?;` to:

```rust
    let config = config::load_lenient(&cli)?;
```

- [ ] **Step 4: Run to verify it passes**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent app_server_mode_uses_a_lenient`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent/src/main.rs yi-agent-rs/crates/yi-agent/src/config.rs
git commit -m "fix(cli): let the app-server start without a model key"
```

---

## Task 7: CLI `yi-agent run` guidance on an unconfigured machine

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs` (`run_headless`)
- Test: `mod tests` in `main.rs`

**Interfaces:**
- Consumes: `yi_agent_runtime::onboarding::{assess, read_env_fields}`.
- Produces: a pure helper `pub(crate) fn unconfigured_guidance(env_path: &Path) -> Option<String>` returning the guidance line when not ready (so it is unit-testable without a subprocess).

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `main.rs`:

```rust
    #[test]
    fn guidance_is_returned_when_the_env_has_no_model() {
        let dir = tempfile::TempDir::new().unwrap();
        let env = dir.path().join(".env");
        let line = unconfigured_guidance(&env).expect("must report an unconfigured machine");
        assert!(line.contains("尚未配置模型"), "got {line}");
        assert!(line.contains("ANTHROPIC_API_KEY"), "got {line}");
    }

    #[test]
    fn no_guidance_when_the_env_is_complete() {
        let dir = tempfile::TempDir::new().unwrap();
        let env = dir.path().join(".env");
        std::fs::write(
            &env,
            "YI_AGENT_PROVIDER=openai\nYI_AGENT_MODEL=gpt-4o\nMODEL_API_KEY=sk-x\n",
        )
        .unwrap();
        assert!(unconfigured_guidance(&env).is_none());
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent unconfigured_guidance`
Expected: FAIL.

- [ ] **Step 3: Implement**

Add near `run_headless` in `main.rs`:

```rust
/// 未配置模型时给 `yi-agent run` 的一句可读指引；就绪则 `None`。
///
/// 非交互入口不能弹问答，但也不该甩 provider 内部错误。
pub(crate) fn unconfigured_guidance(env_path: &std::path::Path) -> Option<String> {
    use yi_agent_runtime::onboarding::{assess, read_env_fields, Assessment};
    let env = read_env_fields(env_path).unwrap_or_default();
    match assess(&env, None) {
        Assessment::Ready { .. } => None,
        Assessment::Needed { .. } => Some(
            "尚未配置模型：请运行 `yi-agent` 完成初始化，或设置环境变量 \
             ANTHROPIC_API_KEY（或 OPENAI_API_KEY）。"
                .to_string(),
        ),
    }
}
```

Then at the very start of `run_headless` (before `config::load`), add:

```rust
    let env_path = config::resolve_env_path(&cli);
    if let Some(line) = unconfigured_guidance(&env_path) {
        eprintln!("{line}");
        std::process::exit(1);
    }
```

(Place it before `let config = config::load(&cli)?;` so the message wins over the raw loader error. Confirm `config::resolve_env_path` is in scope in this file — it is, via `use` already present for `config`.)

- [ ] **Step 4: Run to verify it passes**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent unconfigured_guidance`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent/src/main.rs
git commit -m "feat(cli): print onboarding guidance from yi-agent run"
```

---

## Task 8: CLI TUI first-run Q&A

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs` (`run_agent`)
- Create: `yi-agent-rs/crates/yi-agent/src/onboarding_prompt.rs`
- Test: `mod tests` in `onboarding_prompt.rs` (pure formatting/validation) + `main.rs` dispatch guard

**Interfaces:**
- Consumes: `yi_agent_runtime::onboarding::*`.
- Produces:
  - `pub(crate) fn should_offer(env_path: &Path, preferences_path: &Path) -> bool`
  - `pub(crate) fn provider_defaults(provider: &str) -> (&'static str, &'static str)` → (api_url, model)
  - `pub(crate) fn run_onboarding_prompt(env_path: &Path, prefs_path: &Path) -> Result<()>` (interactive; reads stdin)

- [ ] **Step 1: Write the failing tests**

Create `yi-agent-rs/crates/yi-agent/src/onboarding_prompt.rs`:

```rust
//! TUI 首启的终端问答：把引导收敛成 `ModelSettings` 并写全局 `.env`。

use std::path::Path;

use anyhow::Result;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offers_onboarding_on_a_blank_machine() {
        let dir = tempfile::TempDir::new().unwrap();
        let env = dir.path().join(".env");
        let prefs = dir.path().join("preferences.json");
        assert!(should_offer(&env, &prefs));
    }

    #[test]
    fn does_not_offer_when_the_env_is_complete() {
        let dir = tempfile::TempDir::new().unwrap();
        let env = dir.path().join(".env");
        let prefs = dir.path().join("preferences.json");
        std::fs::write(
            &env,
            "YI_AGENT_PROVIDER=openai\nYI_AGENT_MODEL=gpt-4o\nMODEL_API_KEY=sk-x\n",
        )
        .unwrap();
        assert!(!should_offer(&env, &prefs));
    }

    #[test]
    fn does_not_offer_after_dismissal() {
        let dir = tempfile::TempDir::new().unwrap();
        let env = dir.path().join(".env");
        let prefs = dir.path().join("preferences.json");
        yi_agent_runtime::onboarding::save_dismissed(&prefs, true).unwrap();
        assert!(!should_offer(&env, &prefs));
    }

    #[test]
    fn provider_defaults_cover_both_providers() {
        assert_eq!(provider_defaults("openai"), ("https://api.openai.com", "gpt-4o"));
        assert_eq!(
            provider_defaults("anthropic"),
            ("https://api.anthropic.com", "claude-sonnet-4-20250514")
        );
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent onboarding_prompt`
Expected: FAIL.

- [ ] **Step 3: Implement**

Add to `onboarding_prompt.rs` above the tests. Use plain `std::io` line reads (no new dependency); this matches the crate's dependency-free style:

```rust
/// 是否应自动弹引导：未就绪且用户尚未结束过引导。
pub(crate) fn should_offer(env_path: &Path, preferences_path: &Path) -> bool {
    use yi_agent_runtime::onboarding::{assess, load_dismissed, read_env_fields, Assessment};
    if load_dismissed(preferences_path) {
        return false;
    }
    let env = read_env_fields(env_path).unwrap_or_default();
    matches!(assess(&env, None), Assessment::Needed { .. })
}

/// provider 的默认 (api_url, model)，与 `config.rs` 的默认值同源。
pub(crate) fn provider_defaults(provider: &str) -> (&'static str, &'static str) {
    if provider == "openai" {
        ("https://api.openai.com", "gpt-4o")
    } else {
        ("https://api.anthropic.com", "claude-sonnet-4-20250514")
    }
}

fn read_line(prompt: &str) -> Result<String> {
    use std::io::Write;
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut buf = String::new();
    std::io::stdin().read_line(&mut buf)?;
    Ok(buf.trim().to_string())
}

fn read_line_or(prompt: &str, default: &str) -> Result<String> {
    let value = read_line(prompt)?;
    Ok(if value.is_empty() {
        default.to_string()
    } else {
        value
    })
}

/// 跑一次终端问答并写全局 `.env`。用户中断（EOF）即视为「稍后设置」。
pub(crate) fn run_onboarding_prompt(env_path: &Path, prefs_path: &Path) -> Result<()> {
    use yi_agent_runtime::onboarding::{self, ModelSettings, validate_settings};

    println!("欢迎使用 yi-agent。先配置一个模型就能开始对话。\n");

    let provider = loop {
        let raw = read_line_or("API 格式 [anthropic/openai]（默认 anthropic）: ", "anthropic")?;
        if yi_agent_runtime::models::ModelProvider::parse(&raw).is_some() {
            break raw.to_ascii_lowercase();
        }
        println!("请输入 anthropic 或 openai。");
    };

    let (default_url, default_model) = provider_defaults(&provider);
    let api_key = read_line("API 密钥: ")?;
    if api_key.is_empty() {
        println!("未填密钥，稍后可在设置里补。");
        onboarding::save_dismissed(prefs_path, true)?;
        return Ok(());
    }
    let model = read_line_or(&format!("模型标识（默认 {default_model}）: "), default_model)?;
    let api_url = read_line_or(&format!("API 地址（默认 {default_url}）: "), default_url)?;

    let settings = ModelSettings {
        provider: provider.clone(),
        model: model.clone(),
        api_url: api_url.clone(),
        api_key: api_key.clone(),
    };
    if let Err(error) = validate_settings(&settings) {
        println!("配置无效：{error}");
        onboarding::save_dismissed(prefs_path, true)?;
        return Ok(());
    }

    let test = read_line_or("是否测试连接？[Y/n]（默认 Y）: ", "y")?;
    if !test.eq_ignore_ascii_case("n") {
        println!("正在测试连接…");
        let runtime = tokio::runtime::Runtime::new()?;
        match runtime.block_on(onboarding::test_connection(&settings)) {
            outcome if outcome.ok => println!("连接正常。"),
            outcome => {
                let reason = outcome.reason.unwrap_or_else(|| "未知原因".into());
                let save = read_line_or(&format!("连接失败：{reason}。仍然保存？[Y/n]（默认 Y）: "), "y")?;
                if save.eq_ignore_ascii_case("n") {
                    onboarding::save_dismissed(prefs_path, true)?;
                    return Ok(());
                }
            }
        }
    }

    onboarding::write_model_settings(env_path, &settings)?;
    onboarding::save_dismissed(prefs_path, true)?;
    println!("已保存到 {}", env_path.display());
    Ok(())
}
```

- [ ] **Step 4: Register the module and call it from `run_agent`**

In `main.rs`, add `mod onboarding_prompt;` next to the other `mod` declarations. In `run_agent`, at the top (before `config::load`):

```rust
    let env_path = config::resolve_env_path(&cli);
    let prefs_path = {
        let home = config::resolve_global_env_path()
            .and_then(|p| p.parent().map(|d| d.join("preferences.json")))
            .unwrap_or_else(|| env_path.with_file_name("preferences.json"));
        home
    };
    if onboarding_prompt::should_offer(&env_path, &prefs_path) {
        if let Err(error) = onboarding_prompt::run_onboarding_prompt(&env_path, &prefs_path) {
            eprintln!("引导未完成：{error}");
        }
    }
```

- [ ] **Step 5: Run to verify it passes**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent onboarding_prompt`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent/src/onboarding_prompt.rs yi-agent-rs/crates/yi-agent/src/main.rs
git commit -m "feat(cli): first-run onboarding Q&A in the TUI"
```

---

## Task 9: Desktop onboarding RPC wrappers

**Files:**
- Create: `desktop/src/lib/onboarding.ts`
- Create: `desktop/src/lib/onboarding.test.ts`

**Interfaces:**
- Consumes: host JSON-RPC (`onboarding/status|apply|test|dismiss`).
- Produces:
  - `type OnboardingRpc = <T = unknown>(method: string, params: unknown) => Promise<T>`
  - `type OnboardingStatus = { needed: boolean; dismissed: boolean; reasons: string[] }`
  - `type ApplyInput = { provider: "anthropic" | "openai"; model: string; api_url: string; api_key: string }`
  - `type ApplyResult = { ok: boolean; env_written: boolean; imported: boolean; import_error?: string }`
  - `type TestResult = { ok: boolean; reason: string | null }`
  - `onboardingStatus(rpc)`, `applyOnboarding(rpc, input)`, `testOnboarding(rpc, input)`, `dismissOnboarding(rpc)`

- [ ] **Step 1: Write the failing tests**

Create `desktop/src/lib/onboarding.test.ts`:

```ts
import { describe, expect, it, vi } from "vitest";
import {
  applyOnboarding,
  dismissOnboarding,
  onboardingStatus,
  reasonLabel,
  testOnboarding,
} from "./onboarding";

describe("onboardingStatus", () => {
  it("carries the host payload through", async () => {
    const rpc = vi.fn().mockResolvedValue({ needed: true, dismissed: false, reasons: ["api_key"] });
    await expect(onboardingStatus(rpc)).resolves.toEqual({
      needed: true,
      dismissed: false,
      reasons: ["api_key"],
    });
    expect(rpc).toHaveBeenCalledWith("onboarding/status", {});
  });

  it("defaults a malformed payload to 'not needed'", async () => {
    const rpc = vi.fn().mockResolvedValue({});
    await expect(onboardingStatus(rpc)).resolves.toEqual({
      needed: false,
      dismissed: false,
      reasons: [],
    });
  });
});

describe("applyOnboarding", () => {
  it("sends the four fields and reports the outcome", async () => {
    const rpc = vi.fn().mockResolvedValue({ ok: true, env_written: true, imported: false, import_error: "disk" });
    const result = await applyOnboarding(rpc, {
      provider: "openai",
      model: "gpt-4o",
      api_url: "",
      api_key: "sk-x",
    });
    expect(rpc).toHaveBeenCalledWith("onboarding/apply", {
      provider: "openai",
      model: "gpt-4o",
      api_url: "",
      api_key: "sk-x",
    });
    expect(result).toEqual({ ok: true, env_written: true, imported: false, import_error: "disk" });
  });
});

describe("testOnboarding", () => {
  it("normalises a missing reason to null", async () => {
    const rpc = vi.fn().mockResolvedValue({ ok: true });
    await expect(
      testOnboarding(rpc, { provider: "openai", model: "m", api_url: "", api_key: "k" }),
    ).resolves.toEqual({ ok: true, reason: null });
  });
});

describe("dismissOnboarding", () => {
  it("calls the dismiss method", async () => {
    const rpc = vi.fn().mockResolvedValue({ ok: true });
    await dismissOnboarding(rpc);
    expect(rpc).toHaveBeenCalledWith("onboarding/dismiss", {});
  });
});

describe("reasonLabel", () => {
  it("translates the known missing-field codes to Chinese", () => {
    expect(reasonLabel("api_key")).toBe("未设置 API 密钥");
    expect(reasonLabel("provider")).toBe("未选择 API 格式");
  });

  it("passes an unknown code through unchanged", () => {
    expect(reasonLabel("mystery")).toBe("mystery");
  });
});
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd desktop && npx vitest run src/lib/onboarding.test.ts`
Expected: FAIL — module not found.

- [ ] **Step 3: Implement**

Create `desktop/src/lib/onboarding.ts`:

```ts
/**
 * `onboarding/*` 的桌面端纯封装。
 *
 * 与 `models.ts` 同一层：不碰 React、不碰具体 `RpcClient`，只把「调用意图」
 * 翻成 `method` + `params`，并把宿主回包归一成可判定的形状。明文 key 只
 * 单向发送，从不回读。
 */
export type OnboardingRpc = <T = unknown>(method: string, params: unknown) => Promise<T>;

export type OnboardingStatus = {
  needed: boolean;
  dismissed: boolean;
  reasons: string[];
};

export type ApplyInput = {
  provider: "anthropic" | "openai";
  model: string;
  api_url: string;
  api_key: string;
};

export type ApplyResult = {
  ok: boolean;
  env_written: boolean;
  imported: boolean;
  import_error?: string;
};

export type TestResult = { ok: boolean; reason: string | null };

/** 把宿主给的缺失项代码翻成一句人话（向导首屏据此说清缺什么）。 */
export function reasonLabel(code: string): string {
  switch (code) {
    case "provider":
      return "未选择 API 格式";
    case "model":
      return "未设置模型标识";
    case "api_url":
      return "API 地址格式不正确";
    case "api_key":
      return "未设置 API 密钥";
    default:
      return code;
  }
}

/** 读引导状态。缺字段一律按「不需要引导」处理，绝不因宿主少发字段就弹向导。 */
export async function onboardingStatus(rpc: OnboardingRpc): Promise<OnboardingStatus> {
  const result = await rpc<Partial<OnboardingStatus>>("onboarding/status", {});
  return {
    needed: result?.needed === true,
    dismissed: result?.dismissed === true,
    reasons: Array.isArray(result?.reasons) ? result.reasons : [],
  };
}

/** 写 `.env` 并尝试收边到清单。部分成功（imported=false）也返回 `ok`。 */
export async function applyOnboarding(
  rpc: OnboardingRpc,
  input: ApplyInput,
): Promise<ApplyResult> {
  const result = await rpc<Partial<ApplyResult>>("onboarding/apply", {
    provider: input.provider,
    model: input.model,
    api_url: input.api_url,
    api_key: input.api_key,
  });
  return {
    ok: result?.ok === true,
    env_written: result?.env_written === true,
    imported: result?.imported === true,
    import_error: typeof result?.import_error === "string" ? result.import_error : undefined,
  };
}

/** 探测连接。失败带可读原因；成功 reason 为 null。 */
export async function testOnboarding(rpc: OnboardingRpc, input: ApplyInput): Promise<TestResult> {
  const result = await rpc<{ ok?: unknown; reason?: unknown }>("onboarding/test", {
    provider: input.provider,
    model: input.model,
    api_url: input.api_url,
    api_key: input.api_key,
  });
  return {
    ok: result?.ok === true,
    reason: typeof result?.reason === "string" ? result.reason : null,
  };
}

/** 标记引导已结束（完成或「稍后设置」）。 */
export async function dismissOnboarding(rpc: OnboardingRpc): Promise<void> {
  await rpc("onboarding/dismiss", {});
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cd desktop && npx vitest run src/lib/onboarding.test.ts`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add desktop/src/lib/onboarding.ts desktop/src/lib/onboarding.test.ts
git commit -m "feat(desktop): onboarding RPC wrappers"
```

---

## Task 10: Desktop OnboardingWizard

**Files:**
- Create: `desktop/src/components/OnboardingWizard.tsx`
- Create: `desktop/src/components/OnboardingWizard.test.tsx`

**Interfaces:**
- Consumes: `./lib/onboarding` wrappers.
- Produces: `export function OnboardingWizard({ call, reasons, onDone }: { call?: OnboardingRpc; reasons?: string[]; onDone: () => void })`

Behaviour: steps 欢迎 → 选 API 格式 → 填字段 → （可选）测试 → 完成. The welcome step names what is missing (`reasons`, via `reasonLabel`). Every step has 「稍后设置」 (calls `dismissOnboarding` then `onDone`). A failed test turns the primary button into 「仍然保存」.

- [ ] **Step 1: Write the failing tests**

Create `desktop/src/components/OnboardingWizard.test.tsx`:

```tsx
/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { OnboardingWizard } from "./OnboardingWizard";

afterEach(cleanup);

function renderWizard(call: (method: string, params: unknown) => Promise<unknown>) {
  const onDone = vi.fn();
  render(<OnboardingWizard call={call} onDone={onDone} />);
  return { onDone };
}

describe("OnboardingWizard", () => {
  it("walks from welcome to provider choice to the form", async () => {
    const call = vi.fn(async () => ({ ok: true }));
    renderWizard(call);
    fireEvent.click(screen.getByRole("button", { name: /开始/ }));
    // 选 API 格式这一步
    fireEvent.click(screen.getByRole("button", { name: "openai" }));
    fireEvent.click(screen.getByRole("button", { name: /下一步/ }));
    expect(await screen.findByLabelText("API 密钥")).toBeTruthy();
  });

  it("applies the config and finishes", async () => {
    const call = vi.fn(async (method: string) =>
      method === "onboarding/apply"
        ? { ok: true, env_written: true, imported: true }
        : { ok: true },
    );
    const { onDone } = renderWizard(call);
    fireEvent.click(screen.getByRole("button", { name: /开始/ }));
    fireEvent.click(screen.getByRole("button", { name: "openai" }));
    fireEvent.click(screen.getByRole("button", { name: /下一步/ }));
    fireEvent.change(await screen.findByLabelText("API 密钥"), { target: { value: "sk-x" } });
    fireEvent.change(screen.getByLabelText("模型标识"), { target: { value: "gpt-4o" } });
    fireEvent.click(screen.getByRole("button", { name: /保存/ }));
    await waitFor(() => expect(onDone).toHaveBeenCalled());
    expect(call).toHaveBeenCalledWith("onboarding/apply", {
      provider: "openai",
      model: "gpt-4o",
      api_url: "https://api.openai.com",
      api_key: "sk-x",
    });
  });

  it("offers 'save anyway' after a failed connection test", async () => {
    const call = vi.fn(async (method: string) => {
      if (method === "onboarding/test") return { ok: false, reason: "无法连接 API 地址" };
      return { ok: true, env_written: true, imported: true };
    });
    const { onDone } = renderWizard(call);
    fireEvent.click(screen.getByRole("button", { name: /开始/ }));
    fireEvent.click(screen.getByRole("button", { name: "openai" }));
    fireEvent.click(screen.getByRole("button", { name: /下一步/ }));
    fireEvent.change(await screen.findByLabelText("API 密钥"), { target: { value: "sk-x" } });
    fireEvent.change(screen.getByLabelText("模型标识"), { target: { value: "gpt-4o" } });
    fireEvent.click(screen.getByRole("button", { name: /测试连接/ }));
    expect(await screen.findByText("无法连接 API 地址")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /仍然保存/ }));
    await waitFor(() => expect(onDone).toHaveBeenCalled());
  });

  it("dismisses and finishes when the user chooses 'later'", async () => {
    const call = vi.fn(async () => ({ ok: true }));
    const { onDone } = renderWizard(call);
    fireEvent.click(screen.getByRole("button", { name: /稍后设置/ }));
    await waitFor(() => expect(onDone).toHaveBeenCalled());
    expect(call).toHaveBeenCalledWith("onboarding/dismiss", {});
  });
});
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd desktop && npx vitest run src/components/OnboardingWizard.test.tsx`
Expected: FAIL — module not found.

- [ ] **Step 3: Implement the wizard**

Create `desktop/src/components/OnboardingWizard.tsx`. Follow `SettingsModelsTab`'s seams (injected `call`, no globals) and Tailwind classes already used by that file:

```tsx
import { useState } from "react";
import {
  applyOnboarding,
  dismissOnboarding,
  reasonLabel,
  testOnboarding,
  type ApplyInput,
  type OnboardingRpc,
} from "../lib/onboarding";

type Step = "welcome" | "provider" | "form";

const DEFAULT_URL = {
  anthropic: "https://api.anthropic.com",
  openai: "https://api.openai.com",
} as const;
const DEFAULT_MODEL = {
  anthropic: "claude-sonnet-4-20250514",
  openai: "gpt-4o",
} as const;

export function OnboardingWizard({
  call,
  reasons = [],
  onDone,
}: {
  call?: OnboardingRpc;
  reasons?: string[];
  onDone: () => void;
}) {
  const [step, setStep] = useState<Step>("welcome");
  const [provider, setProvider] = useState<"anthropic" | "openai">("anthropic");
  const [apiKey, setApiKey] = useState("");
  const [model, setModel] = useState("");
  const [apiUrl, setApiUrl] = useState("");
  const [testReason, setTestReason] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const dismiss = async () => {
    try {
      if (call) await dismissOnboarding(call);
    } finally {
      onDone();
    }
  };

  const input = (): ApplyInput => ({
    provider,
    model: model.trim() || DEFAULT_MODEL[provider],
    api_url: apiUrl.trim() || DEFAULT_URL[provider],
    api_key: apiKey,
  });

  const runTest = async () => {
    if (!call) return;
    setBusy(true);
    setTestReason(null);
    setError(null);
    try {
      const result = await testOnboarding(call, input());
      setTestReason(result.ok ? null : result.reason ?? "连接失败");
    } catch (e) {
      setTestReason(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const save = async () => {
    if (!call) return;
    setBusy(true);
    setError(null);
    try {
      await applyOnboarding(call, input());
      await dismissOnboarding(call);
      onDone();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-panel">
      <div className="w-[32rem] rounded-lg border border-line bg-panel p-6 text-fg">
        <div className="flex items-center justify-between">
          <h1 className="text-lg font-medium">欢迎使用 Yi-Agent</h1>
          <button
            type="button"
            onClick={() => void dismiss()}
            className="rounded px-2 py-0.5 text-xs text-fg-subtle hover:text-fg"
          >
            稍后设置
          </button>
        </div>

        {step === "welcome" && (
          <div className="mt-4">
            <p className="text-sm text-fg-muted">
              配置一个模型就能开始对话。所有信息存在本机，只会写入你的全局 .env。
            </p>
            {reasons.length > 0 && (
              <ul className="mt-2 list-disc pl-5 text-xs text-fg-subtle">
                {reasons.map((code) => (
                  <li key={code}>{reasonLabel(code)}</li>
                ))}
              </ul>
            )}
            <button
              type="button"
              onClick={() => setStep("provider")}
              className="mt-4 rounded-md border border-line-strong px-3 py-1.5 text-sm hover:text-fg"
            >
              开始
            </button>
          </div>
        )}

        {step === "provider" && (
          <div className="mt-4">
            <p className="text-sm text-fg-muted">选择 API 格式：</p>
            <div className="mt-2 flex gap-2">
              {(["anthropic", "openai"] as const).map((p) => (
                <button
                  key={p}
                  type="button"
                  onClick={() => setProvider(p)}
                  aria-pressed={provider === p}
                  className={`rounded-md border px-3 py-1.5 text-sm ${
                    provider === p ? "border-line-strong text-fg" : "border-line text-fg-muted"
                  }`}
                >
                  {p}
                </button>
              ))}
            </div>
            <button
              type="button"
              onClick={() => setStep("form")}
              className="mt-4 rounded-md border border-line-strong px-3 py-1.5 text-sm hover:text-fg"
            >
              下一步
            </button>
          </div>
        )}

        {step === "form" && (
          <div className="mt-4 flex flex-col gap-3">
            <label className="flex flex-col gap-1 text-xs text-fg-muted">
              API 密钥
              <input
                type="password"
                value={apiKey}
                onChange={(e) => setApiKey(e.target.value)}
                className="rounded border border-line bg-surface px-2 py-1 text-sm text-fg"
              />
            </label>
            <label className="flex flex-col gap-1 text-xs text-fg-muted">
              模型标识
              <input
                value={model}
                placeholder={DEFAULT_MODEL[provider]}
                onChange={(e) => setModel(e.target.value)}
                className="rounded border border-line bg-surface px-2 py-1 text-sm text-fg"
              />
            </label>
            <label className="flex flex-col gap-1 text-xs text-fg-muted">
              API 地址
              <input
                value={apiUrl}
                placeholder={DEFAULT_URL[provider]}
                onChange={(e) => setApiUrl(e.target.value)}
                className="rounded border border-line bg-surface px-2 py-1 text-sm text-fg"
              />
            </label>

            {testReason !== null && (
              <p role="alert" className="text-xs text-red-400">
                {testReason}
              </p>
            )}
            {error !== null && (
              <p role="alert" className="text-xs text-red-400">
                {error}
              </p>
            )}

            <div className="flex items-center gap-2">
              <button
                type="button"
                onClick={() => void runTest()}
                disabled={busy || !call}
                className="rounded-md border border-line px-3 py-1.5 text-sm text-fg-muted hover:text-fg disabled:opacity-50"
              >
                测试连接
              </button>
              <button
                type="button"
                onClick={() => void save()}
                disabled={busy || !call}
                className="rounded-md border border-line-strong px-3 py-1.5 text-sm hover:text-fg disabled:opacity-50"
              >
                {testReason !== null ? "仍然保存" : "保存"}
              </button>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cd desktop && npx vitest run src/components/OnboardingWizard.test.tsx`
Expected: PASS. Adjust `getByRole("button", { name: ... })` queries if the accessible name differs (e.g. `/开始/` must match the rendered label).

- [ ] **Step 5: Commit**

```bash
cd desktop && npx tsc --noEmit
cd .. && git add desktop/src/components/OnboardingWizard.tsx desktop/src/components/OnboardingWizard.test.tsx
git commit -m "feat(desktop): full-screen onboarding wizard"
```

---

## Task 11: Desktop App wiring (gate on onboarding) + restart sidecar

**Files:**
- Modify: `desktop/src/App.tsx`
- Test: `desktop/src/App.test.tsx`
- Modify: `desktop/src-tauri/src/bridge.rs` + `lib.rs` (only if no restart command exists — verify first)

**Context:** The sidecar caches `cfg` and the catalog at startup, so the newly written `.env` only takes effect after a restart. The desktop already restarts the sidecar when the relay URL changes and re-handshakes on `app-server://status = exited`. Reuse that: add (or reuse) a Tauri command that restarts the sidecar, invoke it after a successful onboarding apply, and let the existing re-handshake path re-run.

**Step 0 (verify before writing):**

Run: `cd desktop/src-tauri && grep -n "restart\|set_relay_url" src/bridge.rs src/lib.rs`
If a restart command already exists, use it. If only `set_relay_url` triggers a restart, extract a `restart_sidecar` Tauri command from the existing `restart_sidecar(app)` function in `bridge.rs` and register it in `lib.rs`'s `invoke_handler`. Record which path you took in the commit message.

- [ ] **Step 1: Write the failing tests**

Add to `desktop/src/App.test.tsx` (follow the existing harness in that file for constructing `App` and a fake transport/RPC):

```tsx
  it("shows the onboarding wizard when the host reports needed", async () => {
    // 让 onboarding/status 回 needed=true，断言向导出现（例如欢迎标题）。
    // 具体接线按本文件既有的假 transport/RPC 模式写。
    // ...arrange: rpc 对 onboarding/status 回 {needed:true,dismissed:false,reasons:[]}
    // ...act: 渲染 App 并等握手完成
    // ...assert: await screen.findByText("欢迎使用 Yi-Agent")
  });

  it("skips the wizard when the host reports ready", async () => {
    // onboarding/status 回 {needed:false,...}，断言向导不出现、主界面可见。
    // ...assert: expect(screen.queryByText("欢迎使用 Yi-Agent")).toBeNull()
  });
```

Fill these in against the file's existing patterns (do not invent a new harness).

- [ ] **Step 2: Run to verify it fails**

Run: `cd desktop && npx vitest run src/App.test.tsx`
Expected: FAIL — the wizard is not rendered yet.

- [ ] **Step 3: Implement the gate**

In `App.tsx`:

1. Import: `import { OnboardingWizard } from "./components/OnboardingWizard";` and `import { onboardingStatus } from "./lib/onboarding";`
2. Add state: `const [onboarding, setOnboarding] = useState<{ needed: boolean; reasons: string[] }>({ needed: false, reasons: [] });`
3. In `handshake`, after `ui/settings/read` succeeds, query status:

```ts
        try {
          const status = await onboardingStatus((m, p) =>
            (clientRef.current as RpcClient).request(m, p),
          );
          setOnboarding({ needed: status.needed && !status.dismissed, reasons: status.reasons });
        } catch {
          // 状态读不到就不弹向导：绝不因为一个读失败挡住主界面。
          setOnboarding({ needed: false, reasons: [] });
        }
```

4. Render the wizard above the main UI:

```tsx
      {onboarding.needed && (
        <OnboardingWizard
          reasons={onboarding.reasons}
          call={(m, p) => (clientRef.current as RpcClient).request(m, p)}
          onDone={() => {
            setOnboarding({ needed: false, reasons: [] });
            // 新配置要重启侧车才生效（cfg/清单在启动时读入）。宿主重启后
            // 会经既有 exited→重新握手路径刷回来。
            void restartSidecar();
            void refreshModels();
          }}
        />
      )}
```

(Use the existing `refreshModels`/equivalent if present; otherwise call `modelCall("model/list", {})` through the shared seam to refresh the settings tab.)

5. `restartSidecar`: call the Tauri command determined in Step 0, guarded for the remote/web client (no Tauri):

```ts
async function restartSidecar() {
  if (isRemoteClient()) return;
  try {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("restart_sidecar");
  } catch {
    // 重启失败不阻塞：配置已在磁盘上，下次启动自然生效。
  }
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cd desktop && npx tsc --noEmit && npx vitest run src/App.test.tsx`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add desktop/src/App.tsx desktop/src/App.test.tsx desktop/src-tauri/src/bridge.rs desktop/src-tauri/src/lib.rs
git commit -m "feat(desktop): gate the main UI behind first-run onboarding"
```

---

## Task 12: Documentation registration

**Files:**
- Modify: `docs/project-management/desktop.md`
- Modify: `docs/project-management/yi-agent-cli.md`
- Modify: `docs/project-management/yi-agent-app-server.md`
- Modify: `docs/project-management/yi-agent-runtime.md`
- Modify: `README.md`

**Interfaces:** none (docs only).

- [ ] **Step 1: Add feature entries**

In each module file, add a `[x]` entry with a **verifiable** completion criterion (code path or command), matching the file's existing style. Examples:

- `yi-agent-runtime.md`: `- [x] 首次安装引导共享逻辑（就绪判定 / 行级保留式 `.env` 写入 / 连接测试 / 结束标记）— yi-agent-rs/crates/yi-agent-runtime/src/onboarding.rs；验证 cargo test -p yi-agent-runtime --lib onboarding`
- `yi-agent-app-server.md`: `- [x] onboarding/* RPC（status 只读需 Observe；apply/test/dismiss 需 Control）— yi-agent-rs/crates/yi-agent-app-server/src/onboarding_rpc.rs；验证 cargo test -p yi-agent-app-server --lib onboarding_`
- `yi-agent-cli.md`: `- [x] TUI 首启问答 + yi-agent run 未配置指引 — yi-agent-rs/crates/yi-agent/src/onboarding_prompt.rs；验证 cargo test -p yi-agent --bin yi-agent onboarding_prompt unconfigured_guidance`
- `desktop.md`: `- [x] 首次启动全屏引导向导 — desktop/src/components/OnboardingWizard.tsx；验证 cd desktop && npx vitest run src/components/OnboardingWizard.test.tsx src/App.test.tsx`

Update the README module-index "完成 / 总计" counts to match.

- [ ] **Step 2: Verify the referenced commands actually pass**

Run each verification command listed above, one crate at a time (per Global Constraints). Fix any entry whose command does not pass.

- [ ] **Step 3: Commit**

```bash
git add docs/project-management/ README.md
git commit -m "docs: register first-run onboarding in project management"
```

---

## Self-Review

**Spec coverage check:**

| Spec section | Task |
|---|---|
| §4.1 shared logic in runtime | Task 1–5 |
| §4.2 assessment (B) | Task 1 |
| §4.3 line-preserving `.env` writer | Task 2 |
| §4.4 connection test (optional, save-anyway) | Task 4 (runtime), Task 10 (UI), Task 8 (CLI) |
| §4.5 dismissal marker | Task 3 |
| §4.6 app-server RPC | Task 6, 6b |
| §4.6 partial-failure honesty (`imported:false`) | Task 6 (`apply`) |
| §4.7 desktop wizard (A) | Task 10, 11 |
| §4.8 CLI TUI Q&A + `run` guidance (A+B) | Task 7, 8 |
| §5 key-less startup (discovered gap) | Task 5, 6c |
| §6 testing strategy | every task's tests |
| §7 relationship to model-catalog-authority | Task 6 (`import_into_catalog`) |

**Type-consistency check:** `ModelSettings`, `EnvModelFields`, `Missing`, `Assessment`, `ConnectionOutcome` are defined in Task 1/2/4 and used with the same names in Tasks 6–8. `OnboardingStatus`/`ApplyInput`/`ApplyResult`/`TestResult` are defined in Task 9 and used in Tasks 10–11. `should_offer`/`provider_defaults`/`run_onboarding_prompt` are defined and consumed in Task 8.

**Known follow-ups (not placeholders, deliberate deferrals per spec §8):** no `yi-agent init` command; no "re-run onboarding" entry; optional vars (Bocha) not in the wizard; connection-test request shape may need a wiremock-verified adjustment in Task 4 Step 5 against the real provider path.
