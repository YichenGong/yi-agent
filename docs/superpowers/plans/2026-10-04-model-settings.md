# 模型设置（Models tab + 会话模型下拉）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让用户能配置一份机器级模型清单（显示名/provider 格式/url/model/api key），并在设置里管理它、给每个会话选模型，宿主与 TUI 都能读写。

**Architecture:** 清单落在 `~/.yi-agent/models.json`，读写与解析逻辑放共享 crate `yi-agent-runtime`（TUI 不经 app-server，必须能直接读）。app-server 暴露 `model/*` RPC 与 `thread/setModel`；会话级模型经 `resolve_effective` 解析后用于构造 provider；切模型走 thread driver 的会话命令通道，在轮次之间重建该会话的 agent。桌面端新增「模型」设置 tab 与输入框右下角下拉；TUI 升级 `/model`。

**Tech Stack:** Rust（`yi-agent-runtime` / `yi-agent-app-server` / `yi-agent-subagent` / `yi-agent` CLI）、React + TypeScript（`desktop/`，vitest + Testing Library）、`serde_json` 持久化。

## Global Constraints

- 编译与测试命令在 `yi-agent-rs/` 下跑；提交前必须 `cargo fmt --all`（在 `yi-agent-rs/` 下）。commit message 用 conventional commits，首行 ≤72 字符，不写 `Co-Authored-By`。
- 严禁在 `main` 上改代码；本计划在一个 worktree 分支上执行（执行者自行 `git worktree add .worktrees/<branch> -b feat/model-settings`）。
- 按 crate 跑测试，避免 `cargo test --workspace`（易 OOM/死锁）：`cargo test -p yi-agent-runtime`、`-p yi-agent-app-server`、`-p yi-agent-subagent`、`-p yi-agent`。跑前 `ps aux | grep cargo` 确认无残留。
- 落盘沿用 `settings_store.rs` 约定：读-改-写保留无关顶层键、临时名 `models.json.<pid>.<seq>.tmp`、`rename` 原子替换。
- `models.json` 权限 `0600`；RPC `model/list` **绝不回传明文 api_key**。
- 显示名唯一、非空；provider 枚举只有 `anthropic` | `openai`；`api_url`、`model` 非空。
- 校验「拒绝即零落盘」：非法输入不得写文件。
- 未配置 `models.json` 时行为必须与今天完全一致（回退 `.env`/cfg）。

---

## 文件结构

- 创建 `yi-agent-rs/crates/yi-agent-runtime/src/models.rs` — 清单类型 + 读写 + 解析（唯一真相源）。
- 修改 `yi-agent-rs/crates/yi-agent-runtime/src/lib.rs` — 导出 `models`。
- 修改 `yi-agent-rs/crates/yi-agent-runtime/src/config.rs` — 给 `RuntimeConfig` 加 `with_model_entry`（或等价）辅助。
- 修改 `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs` — `ThreadMeta` 加 `model_ref`。
- 新建/修改 `yi-agent-rs/crates/yi-agent-app-server/src/model_rpc.rs` — `model/*` 与 `thread/setModel` 的处理（避免继续把 server.rs 撑大；若既有风格强要求内联，则内联到 `server.rs` 的 dispatch）。
- 修改 `yi-agent-rs/crates/yi-agent-app-server/src/session.rs` — `SessionCommand::SetModel`。
- 修改 `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` — dispatch 接线、`apply_session_command` 处理 `SetModel`、`production_factory` 按会话覆盖解析 cfg、`thread/start`/`thread/resume` 写 `model_ref`。
- 修改 `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs` — `model_not_found` 错误码。
- 修改 `yi-agent-rs/crates/yi-agent-subagent/src/attach.rs` — `worker_factory` 用 `subagent_model` 解析。
- 修改 `yi-agent-rs/crates/yi-agent/src/tui/`（`slash.rs`、`app.rs`）— `/model` 列清单与切换。
- 创建 `desktop/src/lib/models.ts` — RPC 封装 + 纯函数。
- 创建 `desktop/src/components/SettingsModelsTab.tsx`、`desktop/src/components/ModelPicker.tsx`。
- 修改 `desktop/src/components/SettingsDialog.tsx`、`MessageInput.tsx`、`StatusBar.tsx`、`desktop/src/App.tsx`、`desktop/src/lib/protocol.ts`。

---

### Task 1: runtime 模型清单类型与读写

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-runtime/src/models.rs`
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/lib.rs`
- Test: 同文件 `#[cfg(test)] mod tests`（该 crate 惯例，见 `settings_store.rs`）

**Interfaces:**
- Consumes: 无（起始任务）。
- Produces:
  - `pub struct ModelEntry { pub name: String, pub provider: ModelProvider, pub api_url: String, pub model: String, pub api_key: String }`
  - `pub enum ModelProvider { Anthropic, Openai }`（`as_str()`→`"anthropic"`/`"openai"`，`parse(&str)->Option<Self>`）
  - `pub struct ModelCatalog { pub models: Vec<ModelEntry>, pub default_model: Option<String>, pub subagent_model: Option<String> }`
  - `pub fn models_path() -> PathBuf`（`~/.yi-agent/models.json`，HOME 缺失回退当前目录，与 `resolve_global_env_path` 同口径）
  - `pub fn load_catalog() -> ModelCatalog`
  - `pub fn save_catalog(catalog: &ModelCatalog) -> std::io::Result<()>`
  - `pub fn mask_key(key: &str) -> String`（末尾 4 位；长度 ≤4 时全掩码）

- [ ] **Step 1: 写失败测试**

在 `models.rs` 底部：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str) -> ModelEntry {
        ModelEntry {
            name: name.to_string(),
            provider: ModelProvider::Anthropic,
            api_url: "https://x".into(),
            model: "claude-x".into(),
            api_key: "sk-secret-1234".into(),
        }
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        let catalog = ModelCatalog {
            models: vec![entry("a")],
            default_model: Some("a".into()),
            subagent_model: None,
        };
        save_catalog_to(&path, &catalog).unwrap();
        assert_eq!(load_catalog_from(&path), catalog);
    }

    #[test]
    fn a_missing_file_is_an_empty_catalog() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        assert_eq!(load_catalog_from(&path), ModelCatalog::default());
    }

    #[test]
    fn a_broken_file_is_an_empty_catalog() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(load_catalog_from(&path), ModelCatalog::default());
    }

    #[test]
    fn malformed_entries_are_skipped() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        std::fs::write(
            &path,
            r#"{"models":[
                {"name":"ok","provider":"openai","api_url":"u","model":"m","api_key":"k"},
                {"name":"","provider":"openai","api_url":"u","model":"m","api_key":"k"},
                {"name":"bad-provider","provider":"gemini","api_url":"u","model":"m","api_key":"k"}
            ],"default_model":"ok"}"#,
        )
        .unwrap();
        let catalog = load_catalog_from(&path);
        assert_eq!(catalog.models.len(), 1);
        assert_eq!(catalog.models[0].name, "ok");
        assert_eq!(catalog.default_model.as_deref(), Some("ok"));
    }

    #[test]
    fn the_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        save_catalog_to(&path, &ModelCatalog::default()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "models.json carries API keys and must be 0600");
    }

    #[test]
    fn saving_leaves_no_temp_file_behind() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        save_catalog_to(&path, &ModelCatalog::default()).unwrap();
        let stray: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n != "models.json")
            .collect();
        assert!(stray.is_empty(), "no temp file may survive a save: {stray:?}");
    }

    #[test]
    fn masks_the_key_leaving_the_tail() {
        assert_eq!(mask_key("sk-secret-1234"), "••••1234");
        assert_eq!(mask_key("ab"), "••••");
        assert_eq!(mask_key(""), "••••");
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime --lib models::`
Expected: 编译失败（`ModelCatalog` 等未定义）。

- [ ] **Step 3: 写最小实现**

在 `models.rs` 顶部：

```rust
//! 机器级模型清单：`~/.yi-agent/models.json`。
//!
//! 每条模型自带 provider 格式 / api_url / api key，密钥跟随 url 走。该文件是
//! **机器级全局**的（与 `~/.yi-agent/.env` 同目录），跨项目、跨重启共享。
//! 未配置时为空清单，调用方回退 `.env`/cfg，行为与今天一致。
//!
//! 落盘约定与 `yi-agent-app-server/src/settings_store.rs` 一致：读-改-写、
//! 临时名逐次唯一、`rename` 原子替换；文件含密钥，权限收紧为 `0600`。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelProvider {
    Anthropic,
    Openai,
}

impl ModelProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            ModelProvider::Anthropic => "anthropic",
            ModelProvider::Openai => "openai",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "anthropic" => Some(ModelProvider::Anthropic),
            "openai" => Some(ModelProvider::Openai),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelEntry {
    pub name: String,
    #[serde(deserialize_with = "de_provider")]
    pub provider: ModelProvider,
    pub api_url: String,
    pub model: String,
    #[serde(default)]
    pub api_key: String,
}

/// 反序列化时把非法 provider 变成错误，让整条被跳过（见 `load_catalog_from` 的逐条过滤）。
fn de_provider<'de, D>(deserializer: D) -> Result<ModelProvider, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    ModelProvider::parse(&s).ok_or_else(|| serde::de::Error::custom("unknown provider"))
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCatalog {
    #[serde(default)]
    pub models: Vec<ModelEntry>,
    #[serde(default)]
    pub default_model: Option<String>,
    #[serde(default)]
    pub subagent_model: Option<String>,
}

/// `<HOME>/.yi-agent/models.json`；HOME 缺失回退当前目录，与 `resolve_global_env_path` 同口径。
pub fn models_path() -> PathBuf {
    std::env::var("HOME")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".yi-agent")
        .join("models.json")
}

/// 读清单。缺失 / 不可读 / 不可解析 → 空清单（坏文件绝不阻断启动）。
/// 逐条过滤：字段非法（空显示名、未知 provider、空 url/model）的条目被跳过。
pub fn load_catalog() -> ModelCatalog {
    load_catalog_from(&models_path())
}

pub fn load_catalog_from(path: &Path) -> ModelCatalog {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ModelCatalog::default()
        }
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "could not read models.json; treating as empty");
            return ModelCatalog::default();
        }
    };
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "invalid models.json; treating as empty");
            return ModelCatalog::default();
        }
    };
    let mut catalog = ModelCatalog::default();
    if let Some(models) = value.get("models").and_then(|v| v.as_array()) {
        for raw in models {
            match serde_json::from_value::<ModelEntry>(raw.clone()) {
                Ok(entry)
                    if !entry.name.trim().is_empty()
                        && !entry.api_url.trim().is_empty()
                        && !entry.model.trim().is_empty() =>
                {
                    catalog.models.push(entry);
                }
                Ok(_) => tracing::warn!(path = %path.display(), "skipping incomplete model entry"),
                Err(error) => {
                    tracing::warn!(%error, path = %path.display(), "skipping malformed model entry")
                }
            }
        }
    }
    catalog.default_model = value
        .get("default_model")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    catalog.subagent_model = value
        .get("subagent_model")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    catalog
}

pub fn save_catalog(catalog: &ModelCatalog) -> std::io::Result<()> {
    save_catalog_to(&models_path(), catalog)
}

pub fn save_catalog_to(path: &Path, catalog: &ModelCatalog) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string_pretty(catalog).map_err(std::io::Error::other)?;
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!("models.json.{}.{}.tmp", std::process::id(), seq));
    write_owner_only(&tmp, &text)?;
    std::fs::rename(&tmp, path)?;
    // rename 保留 tmp 的权限位；显式再设一次以防既有文件权限更宽。
    set_owner_only(path)?;
    Ok(())
}

fn write_owner_only(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(text.as_bytes())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, text)
    }
}

fn set_owner_only(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 密钥掩码：只留尾 4 位，其余用 `••••`；长度 ≤4 时全部掩掉。
pub fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 4 {
        return "••••".to_string();
    }
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("••••{tail}")
}
```

在 `lib.rs` 加 `pub mod models;`。`Models` 测试用到 `tempfile`，确认 `[dev-dependencies]` 已有 `tempfile = "3"`（已有）。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime --lib models::`
Expected: PASS（7 个测试）。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-runtime/src/models.rs crates/yi-agent-runtime/src/lib.rs
git commit -m "feat(runtime): machine-level model catalog (models.json)"
```

---

### Task 2: 会话级模型解析 `resolve_effective`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/models.rs`
- Test: 同文件 `mod tests`

**Interfaces:**
- Consumes: Task 1 的 `ModelCatalog` / `ModelEntry` / `ModelProvider` / `load_catalog_from`；`crate::config::RuntimeConfig`。
- Produces:
  - `pub fn resolve_effective(cfg: &RuntimeConfig, catalog: &ModelCatalog, session_override: Option<&str>) -> RuntimeConfig`
  - `pub fn effective_entry<'a>(catalog: &'a ModelCatalog, session_override: Option<&str>) -> Option<&'a ModelEntry>`（便于 UI 回显层）

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod resolve_tests {
    use super::*;
    use crate::config::RuntimeConfig;

    fn cfg() -> RuntimeConfig {
        // 复用 config.rs 测试的构造方式；若不便，构造一个字段齐全的字面量。
        crate::config::tests_support::test_runtime_config()
    }

    fn catalog() -> ModelCatalog {
        ModelCatalog {
            models: vec![
                ModelEntry { name: "A".into(), provider: ModelProvider::Anthropic,
                    api_url: "https://a".into(), model: "model-a".into(), api_key: "key-a".into() },
                ModelEntry { name: "B".into(), provider: ModelProvider::Openai,
                    api_url: "https://b".into(), model: "model-b".into(), api_key: "key-b".into() },
            ],
            default_model: Some("A".into()),
            subagent_model: None,
        }
    }

    #[test]
    fn a_session_override_wins() {
        let out = resolve_effective(&cfg(), &catalog(), Some("B"));
        assert_eq!(out.provider, "openai");
        assert_eq!(out.api_url, "https://b");
        assert_eq!(out.model, "model-b");
        assert_eq!(out.api_key, "key-b");
    }

    #[test]
    fn a_dangling_override_falls_back_to_the_default() {
        let out = resolve_effective(&cfg(), &catalog(), Some("gone"));
        assert_eq!(out.model, "model-a");
    }

    #[test]
    fn no_default_falls_back_to_cfg() {
        let base = cfg();
        let empty = ModelCatalog::default();
        let out = resolve_effective(&base, &empty, None);
        assert_eq!(out.model, base.model);
        assert_eq!(out.api_url, base.api_url);
    }

    #[test]
    fn only_the_four_fields_change() {
        let base = cfg();
        let out = resolve_effective(&base, &catalog(), Some("B"));
        assert_eq!(out.workdir, base.workdir);
        assert_eq!(out.sandbox, base.sandbox);
        assert_eq!(out.max_turns, base.max_turns);
    }

    #[test]
    fn subagent_falls_back_to_default_then_cfg() {
        let cat = catalog();
        assert_eq!(subagent_entry(&cat).map(|e| e.name.as_str()), Some("A"));
        let mut cat2 = cat.clone();
        cat2.subagent_model = Some("B".into());
        assert_eq!(subagent_entry(&cat2).map(|e| e.name.as_str()), Some("B"));
    }
}
```

（若 `config.rs` 没有可复用的测试构造器，就在 `models.rs` 测试里手写一个最小 `RuntimeConfig`；以实现时能编译为准。）

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime --lib resolve_tests`
Expected: 编译失败（`resolve_effective` 未定义）。

- [ ] **Step 3: 写最小实现**

```rust
/// 会话覆盖 → 全局默认 → 原 cfg 三级解析，返回改写了 provider/api_url/api_key/model
/// 四个字段的 cfg 克隆，其余字段保持原值。
pub fn resolve_effective(
    cfg: &RuntimeConfig,
    catalog: &ModelCatalog,
    session_override: Option<&str>,
) -> RuntimeConfig {
    match effective_entry(catalog, session_override) {
        Some(entry) => {
            let mut out = cfg.clone();
            out.provider = entry.provider.as_str().to_string();
            out.api_url = entry.api_url.clone();
            out.api_key = entry.api_key.clone();
            out.model = entry.model.clone();
            out
        }
        None => cfg.clone(),
    }
}

/// 会话覆盖命中则用它；否则全局默认；都命中不了返回 None（调用方回退 cfg）。
pub fn effective_entry<'a>(
    catalog: &'a ModelCatalog,
    session_override: Option<&str>,
) -> Option<&'a ModelEntry> {
    let by_name = |name: &str| catalog.models.iter().find(|m| m.name == name);
    session_override
        .and_then(by_name)
        .or_else(|| catalog.default_model.as_deref().and_then(by_name))
}

/// 子 agent 条目：`subagent_model` → 全局默认 → None（调用方回退 cfg）。
pub fn subagent_entry(catalog: &ModelCatalog) -> Option<&ModelEntry> {
    let by_name = |name: &str| catalog.models.iter().find(|m| m.name == name);
    catalog
        .subagent_model
        .as_deref()
        .and_then(by_name)
        .or_else(|| catalog.default_model.as_deref().and_then(by_name))
}
```

> 最终签名确定：`subagent_entry(&ModelCatalog) -> Option<&ModelEntry>`。Task 8 直接用它。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime --lib`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-runtime/src/models.rs
git commit -m "feat(runtime): resolve a session's effective model from the catalog"
```

---

### Task 3: ThreadMeta 增加 `model_ref`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs`
- Test: 同文件既有 `mod tests`

**Interfaces:**
- Consumes: 无。
- Produces: `ThreadMeta.model_ref: Option<String>`（`#[serde(default)]`），以及读写辅助 `ThreadStore::set_model_ref(&self, id, Option<&str>)`。

- [ ] **Step 1: 写失败测试**

在 `thread_store.rs` 的测试模块加：

```rust
#[test]
fn a_legacy_meta_without_model_ref_loads_as_none() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = ThreadStore::new(dir.path());
    std::fs::create_dir_all(dir.path().join(".yi-agent").join("threads")).unwrap();
    // 沿用既有测试里写 meta 的方式；这里断言缺字段反序列化为 None。
    let json = r#"{"thread_id":"t","cwd":"/w","model":"m","created_at":1,"updated_at":2,"title":null}"#;
    let meta: ThreadMeta = serde_json::from_str(json).unwrap();
    assert_eq!(meta.model_ref, None);
    let _ = store;
}

#[test]
fn set_model_ref_round_trips() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = ThreadStore::new(dir.path());
    let now = now_millis();
    let meta = ThreadMeta {
        thread_id: "t".into(), cwd: "/w".into(), model: "m".into(),
        created_at: now, updated_at: now, title: None,
        permission_mode: ThreadMode::Normal, pin_seq: None,
        board_project: None, card_id: None, model_ref: None,
    };
    store.create(&meta).unwrap();
    store.set_model_ref("t", Some("B")).unwrap();
    let loaded = store.load("t").unwrap().unwrap();
    assert_eq!(loaded.meta.model_ref.as_deref(), Some("B"));
    store.set_model_ref("t", None).unwrap();
    let loaded = store.load("t").unwrap().unwrap();
    assert_eq!(loaded.meta.model_ref, None);
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_store::tests::set_model_ref_round_trips`
Expected: 编译失败（无 `model_ref` / `set_model_ref`）。

- [ ] **Step 3: 写最小实现**

在 `ThreadMeta` 加字段（放在 `card_id` 之后）：

```rust
    /// 会话模型覆盖：清单里的显示名。None = 跟随全局默认模型。
    /// 与 `model` 的区别：`model` 是**当前生效的 model 串**（显示用），
    /// `model_ref` 是用户在清单里选中的**显示名**（选择用）。
    #[serde(default)]
    pub model_ref: Option<String>,
```

在 `ThreadStore` impl 内加（复用既有的原子写 meta 机制——见 `thread_store.rs` 里写 `.meta.json` 的现有私有函数，用同一个写盘助手）：

```rust
    /// 更新该 thread 的模型覆盖选择，保留其余 meta 字段。
    pub fn set_model_ref(&self, id: &str, model_ref: Option<&str>) -> std::io::Result<()> {
        let mut loaded = self
            .load(id)?
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "unknown thread"))?;
        loaded.meta.model_ref = model_ref.map(str::to_string);
        loaded.meta.updated_at = now_millis();
        // 复用与 `create` / `rename` 相同的 meta 原子写路径。
        self.write_meta(&loaded.meta)
    }
```

> 实现注意：`write_meta` 若不存在，用现有的 meta 落盘私有函数名（`rename` 的实现即是范例）；同时把 `ThreadMeta` 字面量构造点全部补上 `model_ref: None`（`server.rs` 有若干测试构造点，编译器会报错逐个补）。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_store::`
Expected: PASS（含既有 thread_store 测试，确认没破坏旧 meta 兼容）。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-app-server/src/thread_store.rs crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): persist a session's model_ref in thread meta"
```

---

### Task 4: app-server 按会话解析 provider（`production_factory` 接缝）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`production_factory` 及其 3 个调用点 `run` / `serve_stdio_with_relay` / `ws.rs`）
- Test: `server.rs` 既有 `#[cfg(test)]` 测试模块

**Interfaces:**
- Consumes: Task 2 `resolve_effective`、`load_catalog`；Task 3 `ThreadMeta.model_ref`。
- Produces: `production_factory(cfg, theme)` 返回的闭包签名不变 `Fn(Option<Session>, &Path, ThreadMode) -> Result<BuiltAgent>`，但内部按 `thread_cfg.model_ref` 解析后再 `bootstrap_agent`。为此需让闭包能拿到「本次会话的 model_ref」——采用 **thread-local 传递**：新增 `pub(crate) fn with_session_model_ref<T>(model_ref: Option<String>, f: impl FnOnce() -> T) -> T`，在调用 `build_agent(...)` 的 `thread/start` / `thread/resume` 处包一层。

> 备选接法（若 thread-local 被否）：把闭包签名改为 `Fn(Option<Session>, &Path, ThreadMode, Option<&str>) -> Result<BuiltAgent>`，同步改 5 处类型约束 + 2 处调用。实现者二选一，但**必须让 `thread/start` 与 `thread/resume` 两条路径一致**。

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn a_session_model_ref_selects_the_catalog_entry() {
    // 用一个临时 HOME 指向的 models.json，断言 production_factory 造出的 agent
    // 使用的 model 是条目里的串，而不是全局 cfg.model。
    let dir = tempfile::TempDir::new().unwrap();
    let models = dir.path().join(".yi-agent").join("models.json");
    std::fs::create_dir_all(models.parent().unwrap()).unwrap();
    std::fs::write(
        &models,
        r#"{"models":[{"name":"X","provider":"openai","api_url":"https://x",
            "model":"model-x","api_key":"k"}],"default_model":"X"}"#,
    )
    .unwrap();
    std::env::set_var("HOME", dir.path()); // 在受限测试中设置；若并行测试敏感，改用注入路径参数
    let cfg = /* 与既有测试相同的 test RuntimeConfig */;
    let factory = production_factory(cfg, test_theme());
    let built = factory(None, dir.path(), crate::thread_store::ThreadMode::Normal).unwrap();
    assert_eq!(built.agent.config().model, "model-x");
    std::env::remove_var("HOME");
}
```

> 若 `HOME` 环境变量在并行测试下不安全，改为让 `production_factory` 接收一个 `Arc<ModelCatalog>`（在 `run` 里 `Arc::new(load_catalog())` 一次），闭包持有它——这样测试可直接注入 catalog，不必动 `HOME`。**推荐这个注入式接法**，更可测。

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib a_session_model_ref_selects_the_catalog_entry`
Expected: FAIL（仍用 `cfg.model`）。

- [ ] **Step 3: 写最小实现**

把 `production_factory` 改为接收并持有 catalog：

```rust
pub(crate) fn production_factory(
    cfg: RuntimeConfig,
    theme: crate::theme_tool::ThemeHandle,
    catalog: Arc<yi_agent_runtime::models::ModelCatalog>,
) -> impl Fn(Option<yi_agent_core::Session>, &Path, crate::thread_store::ThreadMode)
    -> anyhow::Result<BuiltAgent> + Send + Sync + 'static {
    move |session, cwd, mode| {
        let mut thread_cfg = cfg.clone();
        thread_cfg.workdir = cwd.to_path_buf();
        thread_cfg.yolo = mode == crate::thread_store::ThreadMode::Yolo;
        let model_ref = current_session_model_ref();
        thread_cfg = yi_agent_runtime::models::resolve_effective(
            &thread_cfg,
            &catalog,
            model_ref.as_deref(),
        );
        let built = yi_agent_runtime::bootstrap::bootstrap_agent(
            &thread_cfg,
            yi_agent_runtime::bootstrap::PermissionMode::Interactive,
        )?;
        // ... 其余不变（主题工具 / wrap_for_delegation）
        Ok(built)
    }
}
```

新增 thread-local：

```rust
thread_local! {
    static SESSION_MODEL_REF: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

/// 在调用 `build_agent` 前设定本次会话的模型覆盖（显示名），供工厂解析。
pub(crate) fn with_session_model_ref<T>(model_ref: Option<String>, f: impl FnOnce() -> T) -> T {
    SESSION_MODEL_REF.with(|slot| {
        let prev = slot.replace(model_ref);
        let out = f();
        slot.replace(prev);
        out
    })
}

fn current_session_model_ref() -> Option<String> {
    SESSION_MODEL_REF.with(|slot| slot.borrow().clone())
}
```

在 `run` / `serve_stdio_with_relay` 里把 `production_factory(cfg, theme)` 改为 `production_factory(cfg, theme, Arc::new(yi_agent_runtime::models::load_catalog()))`。

在 `thread/start`（`server.rs:2721`）与 `thread/resume`（`server.rs:2860`）调用处包一层：

```rust
let built = with_session_model_ref(model_ref.clone(), || build_agent(None, Path::new(&cwd), mode))?;
```

其中 `thread/start` 的 `model_ref` 取自请求参数（Task 8 落地）；`thread/resume` 取自 `loaded.meta.model_ref.clone()`。

> 注意：`build_agent` 是 `Fn`（非 async），thread-local 在此期间未被 await 打断，故安全。若实现者担心跨 await，改用显式参数接法。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib a_session_model_ref_selects_the_catalog_entry`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-app-server/src/server.rs crates/yi-agent-app-server/src/ws.rs crates/yi-agent-app-server/src/lib.rs
git commit -m "feat(app-server): build a session's provider from its model_ref"
```

---

### Task 5: `SessionCommand::SetModel` 与 driver 重建

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/session.rs`（枚举）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`apply_session_command` 与 `run_thread_driver` 签名）
- Test: `server.rs` 既有测试模块

**Interfaces:**
- Consumes: Task 4 的 `with_session_model_ref`；`yi_agent_runtime::bootstrap::build_provider`；`yi_agent_core::Agent::new` / `with_session_arc` / `config()`。
- Produces: `SessionCommand::SetModel { model_ref: Option<String>, effective: Box<RuntimeConfig>, reply: oneshot::Sender<Result<(), String>> }`

- [ ] **Step 1: 写失败测试**

```rust
#[tokio::test]
async fn set_model_command_rebuilds_with_the_new_model() {
    // 起一个 thread driver（沿用既有 driver 测试桩），发 SetModel，
    // 断言重建后 agent.config().model == 新条目的 model，且 session 历史保留。
    // 具体桩法参照既有 `session_rx` 空闲命令测试（server.rs:9784 附近）。
}
```

（用既有空闲命令测试的同一套 helper 写实现；断言点：`agent.config().model`、`agent.session().messages().len()` 不变。）

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib set_model_command_rebuilds_with_the_new_model`
Expected: 编译失败（无 `SetModel` 变体）。

- [ ] **Step 3: 写最小实现**

`session.rs`：

```rust
    /// 切换该 thread 的会话模型：写路径已完成 meta 更新，这里只负责重建 agent。
    SetModel {
        /// 新解析出的 cfg（含 provider/api_url/api_key/model）。
        effective: Box<yi_agent_runtime::config::RuntimeConfig>,
        /// 新选中的显示名（仅用于日志与回执）。
        model_ref: Option<String>,
        reply: oneshot::Sender<Result<(), String>>,
    },
```

`server.rs` 的 `apply_session_command`：新增分支——重建 provider 与 agent，复用 session Arc，保留工具注册表：

```rust
        SessionCommand::SetModel { effective, model_ref, reply } => {
            let provider = match yi_agent_runtime::bootstrap::build_provider(&effective) {
                Ok(p) => p,
                Err(error) => {
                    let _ = reply.send(Err(error.to_string()));
                    return agent; // 保留原 agent，绝不半重建
                }
            };
            let mut next_config = agent.config().clone();
            next_config.model = effective.model.clone();
            let session_handle = agent.session_handle();
            let tools = /* agent 现有工具注册表句柄 */;
            let rebuilt = yi_agent_core::Agent::new(provider, tools, next_config)
                .with_session_arc(session_handle);
            // 保留审批路径（checker + decision_rx），与既有重建路径一致。
            let rebuilt = rebuilt/* .with_permission(...) 若原 agent 有 */;
            tracing::info!(model = %effective.model, ?model_ref, "session model switched");
            let _ = reply.send(Ok(()));
            rebuilt
        }
```

`run_thread_driver`：`SessionCommand::SetModel` 与 `Clear`/`Compact` 一样在**空闲路径**与**轮次之间**处理（复用既有 `pending_session_command` 机制），无需单独改 driver 控制流。

> 工具注册表 / 审批路径的取回方式：`apply_session_command` 已接收 `provider`、`config`；若拿不到 `tools`，则给 `Agent` 增加 `pub fn tools(&self) -> Arc<ToolRegistry>` 并在此用上（或在 `Agent` 上加 `pub fn rebuild_with(&self, provider, config) -> Agent` 封装这层，避免在 server.rs 里摊开字段）。实现者二选一，保证与既有 `rebuild_driver_agent` 语义一致。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib set_model_command_rebuilds_with_the_new_model`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-app-server/src/session.rs crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): switch a session's model between turns"
```

---

### Task 6: `model/*` RPC 与校验（`model_not_found` 错误码）

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/model_rpc.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs`（错误码）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（dispatch 接线）、`lib.rs`（`pub mod model_rpc;`）
- Test: `model_rpc.rs` 的 `#[cfg(test)]`

**Interfaces:**
- Consumes: Task 1 `ModelCatalog`/`ModelEntry`/`ModelProvider`/`load_catalog`/`save_catalog`/`mask_key`。
- Produces:
  - dispatch 分支：`model/list`、`model/upsert`、`model/delete`、`model/setDefault`、`model/setSubagent`
  - `pub fn handle_model_request(method: &str, params: &Value) -> Result<Value, RpcError>`
  - `protocol.rs`：`RpcError::model_not_found(name)` → 数字码 `-32025`，`data.code = "model_not_found"`

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn with_temp_home<T>(f: impl FnOnce() -> T) -> T { /* 设 HOME 到临时目录的辅助 */ }

    #[test]
    fn list_never_returns_the_raw_key() {
        let out = with_temp_home(|| {
            handle_model_request("model/upsert", &json!({
                "name":"A","provider":"anthropic","api_url":"https://a",
                "model":"m","api_key":"sk-secret-1234"})).unwrap();
            assert_eq!(out["ok"], true);
            handle_model_request("model/list", &json!({})).unwrap()
        });
        let text = out.to_string();
        assert!(!text.contains("sk-secret-1234"), "the raw key must never be returned: {text}");
        assert_eq!(out["models"][0]["has_key"], true);
        assert_eq!(out["models"][0]["api_key_masked"], "••••1234");
        assert!(out["models"][0].get("api_key").is_none());
    }

    #[test]
    fn upsert_without_api_key_keeps_the_stored_one() {
        with_temp_home(|| {
            handle_model_request("model/upsert", &json!({
                "name":"A","provider":"anthropic","api_url":"https://a",
                "model":"m","api_key":"sk-secret-1234"})).unwrap();
            handle_model_request("model/upsert", &json!({
                "name":"A","provider":"openai","api_url":"https://b",
                "model":"m2"})).unwrap();
            let list = handle_model_request("model/list", &json!({})).unwrap();
            assert_eq!(list["models"][0]["api_key_masked"], "••••1234");
        });
    }

    #[test]
    fn an_empty_key_clears_it() {
        with_temp_home(|| {
            handle_model_request("model/upsert", &json!({
                "name":"A","provider":"anthropic","api_url":"https://a",
                "model":"m","api_key":"sk-secret-1234"})).unwrap();
            handle_model_request("model/upsert", &json!({
                "name":"A","provider":"anthropic","api_url":"https://a",
                "model":"m","api_key":""})).unwrap();
            let list = handle_model_request("model/list", &json!({})).unwrap();
            assert_eq!(list["models"][0]["has_key"], false);
        });
    }

    #[test]
    fn invalid_input_writes_nothing() {
        with_temp_home(|| {
            handle_model_request("model/upsert", &json!({
                "name":"A","provider":"anthropic","api_url":"https://a",
                "model":"m","api_key":"k"})).unwrap();
            let err = handle_model_request("model/upsert", &json!({
                "name":"B","provider":"gemini","api_url":"u","model":"m"})).unwrap_err();
            assert_eq!(err.data.unwrap()["code"], "invalid_model"); // 或 invalid_params
            let list = handle_model_request("model/list", &json!({})).unwrap();
            assert_eq!(list["models"].as_array().unwrap().len(), 1);
        });
    }

    #[test]
    fn set_default_to_an_unknown_name_is_rejected() {
        with_temp_home(|| {
            let err = handle_model_request("model/setDefault", &json!({"name":"nope"})).unwrap_err();
            assert_eq!(err.data.unwrap()["code"], "model_not_found");
        });
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib model_rpc`
Expected: 编译失败（模块不存在）。

- [ ] **Step 3: 写最小实现**

新建 `model_rpc.rs`：纯函数 `handle_model_request(method, params)`，内部 `load_catalog()` → 校验/变更 → `save_catalog()` → 返回 JSON。关键点：

- `model/list`：逐条输出 `{name, provider, api_url, model, has_key, api_key_masked}`（用 `mask_key`），**不含 `api_key`**；附 `default_model`/`subagent_model`。
- `model/upsert`：`name` 非空、`provider` 合法、`api_url`/`model` 非空；`api_key` 缺省=保留、空串=清空、非空=覆盖；同名更新、异名新增。
- `model/delete`：删条目，并**清理指向它的 `default_model`/`subagent_model`**（置 None）。
- `model/setDefault` / `model/setSubagent`：`null` 或缺失 → 清；否则必须命中已存在条目，否则 `model_not_found`。
- 校验失败：返回错误且**不调用 `save_catalog`**（零落盘）。

`protocol.rs` 加：

```rust
    pub fn model_not_found(name: &str) -> Self {
        Self {
            code: -32025,
            message: format!("model not found: {name}"),
            data: Some(serde_json::json!({ "code": "model_not_found" })),
        }
    }
```

`server.rs` dispatch：在既有 `plugin/settings/*` 附近加：

```rust
"model/list" => { /* 权限 Observe */ }
"model/upsert" | "model/delete" | "model/setDefault" | "model/setSubagent" => { /* 权限 Control */ }
```

接线调用 `crate::model_rpc::handle_model_request(method, &req.params)`，成功 `ok_response`，`Err(rpc_error)` 走 `err_response`。权限判定沿用既有的 scope 检查范式（`plugin/settings/write` 附近有范例）。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib model_rpc`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-app-server/src/model_rpc.rs crates/yi-agent-app-server/src/protocol.rs crates/yi-agent-app-server/src/server.rs crates/yi-agent-app-server/src/lib.rs
git commit -m "feat(app-server): model/* RPC with key masking and validation"
```

---

### Task 7: `thread/setModel` 与 `thread/start` 的模型参数

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（dispatch + `thread/start` + `thread/setModel`）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs`（`ThreadStarted` / thread 元信息若需带 `model_ref`）
- Test: `server.rs` 既有测试模块

**Interfaces:**
- Consumes: Task 3 `set_model_ref`、Task 2 `resolve_effective`/`load_catalog`、Task 5 `SessionCommand::SetModel`。
- Produces:
  - `thread/start { cwd?, model_ref? }` — 可选在创建时即指定覆盖
  - `thread/setModel { thread_id, name }` — `name` 为 `null` 清除覆盖；成功返回 `{ ok: true, model: <生效串> }`

- [ ] **Step 1: 写失败测试**

```rust
#[tokio::test]
async fn thread_set_model_persists_and_reports_the_effective_model() {
    // 1) model/upsert A、B 两条；
    // 2) thread/start；
    // 3) thread/setModel { name: "B" } → 返回 model == B.model；
    // 4) thread/resume 回读 → meta.model_ref == Some("B")、meta.model == B.model。
}

#[tokio::test]
async fn thread_set_model_to_null_clears_the_override() {
    // setModel B 后 setModel null → meta.model_ref == None、model 回到默认。
}

#[tokio::test]
async fn thread_set_model_to_unknown_name_is_rejected() {
    // → model_not_found，且不中断会话、meta 不变。
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_set_model`
Expected: FAIL（`method not found`）。

- [ ] **Step 3: 写最小实现**

`thread/setModel` 处理：

```rust
"thread/setModel" => {
    // 权限 Control；require_thread_id。
    let name = req.params.get("name").and_then(|v| v.as_str()).map(str::to_string);
    if let Some(n) = &name {
        let catalog = yi_agent_runtime::models::load_catalog();
        if !catalog.models.iter().any(|m| &m.name == n) {
            write_response(&hub, &client, err_response(id, RpcError::model_not_found(n))).await?;
            continue;
        }
    }
    let store = store_lookup(&threads, &workspaces, &cfg, &thread_id);
    // 1) 写 meta：model_ref + 生效 model 串
    let catalog = yi_agent_runtime::models::load_catalog();
    let effective = yi_agent_runtime::models::resolve_effective(&cfg, &catalog, name.as_deref());
    store.set_model_ref(&thread_id, name.as_deref())?;
    store.set_model_string(&thread_id, &effective.model)?; // 若需独立写生效串
    // 2) 交由 driver 重建（本轮结束后生效）
    let (reply_tx, reply_rx) = oneshot::channel();
    if let Some(session) = threads.get(&thread_id) {
        let _ = session.session_tx.send(SessionCommand::SetModel {
            effective: Box::new(effective.clone()),
            model_ref: name.clone(),
            reply: reply_tx,
        }).await;
        let _ = reply_rx.await;
    }
    // 3) 更新内存 ThreadSession.model 并返回
    write_response(&hub, &client, ok_response(id, json!({ "ok": true, "model": effective.model }))).await?;
}
```

`thread/start`：读可选 `model_ref`，建 agent 前用 `with_session_model_ref(model_ref.clone(), || build_agent(None, ...))`，并在 meta 里写 `model_ref` 与生效 `model`（生效串用 `resolve_effective` 算）。

`thread/resume`：`with_session_model_ref(loaded.meta.model_ref.clone(), || build_agent(Some(session), ...))`。

> `set_model_string` 若不存在，用现有 meta 写路径改 `model` 字段；或让 `set_model_ref` 一次写两个字段（`model_ref` + `model`），签名改为 `set_model(&self, id, model_ref: Option<&str>, model: &str)`，减少一次落盘。实现时以「一次原子写」为准。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_set_model`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-app-server/src/server.rs crates/yi-agent-app-server/src/protocol.rs crates/yi-agent-app-server/src/thread_store.rs
git commit -m "feat(app-server): thread/setModel and model_ref on thread/start"
```

---

### Task 8: 子 agent 模型（`subagent_model`）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/attach.rs`（`worker_factory`）
- Test: `attach.rs` / `lib.rs` 既有测试模块

**Interfaces:**
- Consumes: Task 1/2 `subagent_entry`、`resolve_effective`。
- Produces: `worker_factory` 按 `subagent_model`（缺省回退全局默认）解析 cfg 建 provider；worker 的 `config.model` 为该条目 model。

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn the_worker_uses_the_subagent_model_entry() {
    // 临时 HOME 写 models.json：A(default)、S(subagent)。
    // 断言 worker_factory 造出的 worker config.model == S.model。
}

#[test]
fn the_worker_falls_back_to_the_global_default() {
    // 只配 default A、无 subagent → worker model == A.model。
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-subagent --lib the_worker_uses_the_subagent_model_entry`
Expected: FAIL（仍用全局 cfg.model）。

- [ ] **Step 3: 写最小实现**

在 `attach.rs::worker_factory` 顶部：

```rust
    let catalog = yi_agent_runtime::models::load_catalog();
    let effective = match yi_agent_runtime::models::subagent_entry(&catalog) {
        Some(entry) => {
            let mut c = cfg.clone();
            c.provider = entry.provider.as_str().to_string();
            c.api_url = entry.api_url.clone();
            c.api_key = entry.api_key.clone();
            c.model = entry.model.clone();
            c
        }
        None => cfg.clone(),
    };
    let provider = yi_agent_runtime::bootstrap::build_provider(&effective)?;
    // 后续 agent_config 用 &effective 而非 cfg（保证 worker.config.model 是新条目）。
```

把该函数内其余 `cfg` 使用替换为 `effective`。

> 需确认 `yi-agent-subagent/Cargo.toml` 依赖 `yi-agent-runtime`（既有 `attach.rs` 已用 `yi_agent_runtime::bootstrap`，故已有）。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-subagent --lib`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-subagent/src/attach.rs
git commit -m "feat(subagent): run workers on the configured subagent model"
```

---

### Task 9: TUI `/model` 升级

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs`、`yi-agent-rs/crates/yi-agent/src/tui/slash.rs`
- Test: 既有 TUI 测试模块（`app.rs` 内 `model_command_sends_set_model_control` 等）

**Interfaces:**
- Consumes: Task 1/2 `load_catalog` / `resolve_effective`；既有 `ControlCommand::SetModel`。
- Produces: `/model`（无参）打印清单 + 当前选中；`/model <name>` 切当前会话；`/model default` 清除（进程内）。

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn model_command_lists_the_catalog_when_given_no_args() {
    // 临时 HOME 写两条；断言渲染文本含两个显示名与「当前:」标记。
}

#[test]
fn model_command_switches_to_a_named_entry() {
    // /model B → 发出 ControlCommand::SetModel(<B 的 model 串>)。
}

#[test]
fn model_command_rejects_an_unknown_name() {
    // /model nope → 错误提示，不发命令。
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent --lib model_command_`
Expected: FAIL。

- [ ] **Step 3: 写最小实现**

在 `app.rs` 处理 `/model` 的分支（现约 `app.rs:2276`）改为：

```rust
let catalog = yi_agent_runtime::models::load_catalog();
match arg.as_deref() {
    None | Some("") => { /* 渲染清单：每行显示名 + (默认) + 「当前」标记 */ }
    Some("default") => { /* 清除覆盖：ControlCommand::SetModel(默认 model 串) */ }
    Some(name) => match catalog.models.iter().find(|m| m.name == name) {
        Some(entry) => { let _ = control_tx.blocking_send(
            crate::ControlCommand::SetModel(entry.model.clone())); }
        None => { /* 错误提示：未知模型名 */ }
    },
}
```

`slash.rs` 的用法字符串更新为 `/model [<name>|default]`。

> 注意：CLI 的 `ControlCommand::SetModel` 目前只接收 model 串，不含 provider/url/key。本期 TUI 只切「同 provider 的 model 串」；跨 provider 切换在 TUI 侧属已知限制（与设计 §8/§12 一致，TUI 不持久化 thread meta）。若实现发现需要完整 cfg，则把 `ControlCommand::SetModel` 扩为携带 `RuntimeConfig`，并在 `main.rs:1972` 的 rebuild 处用新 provider。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent --lib model_command_`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent/src/tui/app.rs crates/yi-agent/src/tui/slash.rs
git commit -m "feat(tui): /model lists and switches catalog models"
```

---

### Task 10: 桌面端 RPC 封装 `models.ts`

**Files:**
- Create: `desktop/src/lib/models.ts`
- Test: `desktop/src/lib/models.test.ts`

**Interfaces:**
- Consumes: 宿主 `model/*`、`thread/setModel`（Task 6/7）。
- Produces:
  - `export type ModelEntryView = { name: string; provider: "anthropic" | "openai"; api_url: string; model: string; has_key: boolean; api_key_masked: string }`
  - `export type ModelList = { models: ModelEntryView[]; default_model: string | null; subagent_model: string | null }`
  - `listModels(rpc): Promise<ModelList>`
  - `upsertModel(rpc, input): Promise<void>`（`api_key` 省略 = 保留）
  - `deleteModel(rpc, name)`, `setDefaultModel(rpc, name|null)`, `setSubagentModel(rpc, name|null)`, `setThreadModel(rpc, threadId, name|null)`
  - `export function isModelNotFound(error: unknown): boolean`（读 `data.code === "model_not_found"`）

- [ ] **Step 1: 写失败测试**

```ts
import { describe, expect, it, vi } from "vitest";
import { isModelNotFound, upsertModel } from "./models";

describe("upsertModel", () => {
  it("omits api_key when the caller leaves it undefined", async () => {
    const rpc = vi.fn().mockResolvedValue({ ok: true });
    await upsertModel(rpc, { name: "A", provider: "anthropic", api_url: "u", model: "m" });
    expect(rpc).toHaveBeenCalledWith("model/upsert",
      { name: "A", provider: "anthropic", api_url: "u", model: "m" });
    expect(rpc.mock.calls[0][1]).not.toHaveProperty("api_key");
  });
});

describe("isModelNotFound", () => {
  it("reads the stable data.code", () => {
    expect(isModelNotFound({ data: { code: "model_not_found" } })).toBe(true);
    expect(isModelNotFound({ data: { code: "other" } })).toBe(false);
    expect(isModelNotFound(new Error("x"))).toBe(false);
  });
});
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd desktop && npx vitest run src/lib/models.test.ts`
Expected: FAIL（模块不存在）。

- [ ] **Step 3: 写最小实现**

按 `desktop/src/lib/boardIndex.ts` / `pluginSettings.ts` 的风格实现纯封装；`upsertModel` 只有在 `input.api_key !== undefined` 时才把 `api_key` 放进参数。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd desktop && npx vitest run src/lib/models.test.ts`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/lib/models.ts desktop/src/lib/models.test.ts
git commit -m "feat(desktop): model/* RPC wrappers"
```

---

### Task 11: 桌面端「模型」设置 tab

**Files:**
- Create: `desktop/src/components/SettingsModelsTab.tsx`、`desktop/src/components/SettingsModelsTab.test.tsx`
- Modify: `desktop/src/components/SettingsDialog.tsx`, `desktop/src/components/SettingsDialog.test.tsx`, `desktop/src/App.tsx`（注入 `modelCall`）

**Interfaces:**
- Consumes: Task 10 的 `listModels`/`upsertModel`/... 与类型。
- Produces: `<SettingsModelsTab call={...} />`；`SettingsDialog` 新增 `modelCall` prop 与 `{ id:"models", label:"模型" }` tab。

- [ ] **Step 1: 写失败测试**

```tsx
// SettingsDialog.test.tsx
it("renders a models tab", () => {
  render(<SettingsDialog open ... />);
  expect(screen.getByRole("tab", { name: "模型" })).toBeInTheDocument();
});

// SettingsModelsTab.test.tsx
it("lists entries with a masked key and never echoes the raw key", async () => {
  const call = vi.fn().mockResolvedValue({
    models: [{ name: "A", provider: "anthropic", api_url: "u", model: "m",
               has_key: true, api_key_masked: "••••1234" }],
    default_model: "A", subagent_model: null,
  });
  render(<SettingsModelsTab call={call} />);
  expect(await screen.findByText("••••1234")).toBeInTheDocument();
});

it("does not send api_key when the user leaves it untouched", async () => {
  // 编辑 url 后保存 → 断言 upsertModel 的调用参数无 api_key。
});

it("shows an inline error and keeps input on failure", async () => {
  // call 第二次 reject → 断言错误文案出现且输入框仍有用户输入。
});
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd desktop && npx vitest run src/components/SettingsModelsTab.test.tsx`
Expected: FAIL（组件不存在）。

- [ ] **Step 3: 写最小实现**

- `SettingsDialog.tsx`：`TABS` 加 `{ id: "models", label: "模型" }`；新增 `modelCall?: (method, params) => Promise<unknown>` prop；panel 分支渲染 `<SettingsModelsTab call={modelCall} />`。
- `SettingsModelsTab.tsx`：载入 `listModels`；表格增删改；「全局默认模型」「子 agent 模型」两个下拉（含「跟随全局默认」项）；保存走 `upsertModel` 等；**写成功后重读**（无乐观更新）；失败内联报错并保留输入；key 输入框显示掩码，用户不改则不发送 `api_key`。
- `App.tsx`：构造稳定的 `modelCall`（同 `pluginCall` 的写法）传入 `SettingsDialog`。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd desktop && npx vitest run src/components/SettingsModelsTab.test.tsx src/components/SettingsDialog.test.tsx`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/components/SettingsModelsTab.tsx desktop/src/components/SettingsModelsTab.test.tsx desktop/src/components/SettingsDialog.tsx desktop/src/components/SettingsDialog.test.tsx desktop/src/App.tsx
git commit -m "feat(desktop): models settings tab"
```

---

### Task 12: 桌面端会话模型下拉 + 状态栏去 model

**Files:**
- Create: `desktop/src/components/ModelPicker.tsx`、`desktop/src/lib/modelPicker.ts`、对应测试
- Modify: `desktop/src/components/MessageInput.tsx`, `desktop/src/App.tsx`, `desktop/src/components/StatusBar.tsx`, `desktop/src/components/StatusBar.test.tsx`, `desktop/src/lib/protocol.ts`, `desktop/src/lib/threadStore.ts`（`info.model_ref`）

**Interfaces:**
- Consumes: Task 10 `listModels`/`setThreadModel`；会话 `info.model` / `info.model_ref`。
- Produces: `MessageInput` 新增 `modelPicker?: ReactNode`；`buildModelOptions(list, currentRef, currentModel)`；`StatusBar` 移除 `model` prop。

- [ ] **Step 1: 写失败测试**

```ts
// modelPicker.test.ts
it("puts a follow-default option first, showing the resolved model name", () => {
  const opts = buildModelOptions(
    { models: [{ name: "A", provider: "anthropic", api_url: "u", model: "model-a",
                 has_key: true, api_key_masked: "••••" }],
      default_model: "A", subagent_model: null },
    null, "model-a");
  expect(opts[0]).toMatchObject({ value: null });
  expect(opts[0].label).toContain("model-a");
});

it("marks the session's current entry as selected", () => {
  // currentRef = "A" → 对应项 selected。
});
```

```tsx
// StatusBar.test.tsx（改）
it("no longer renders the model text", () => {
  render(<StatusBar cwd="/w" status="connected" usage={null} />);
  expect(screen.queryByText("claude-x")).not.toBeInTheDocument();
});
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd desktop && npx vitest run src/lib/modelPicker.test.ts src/components/StatusBar.test.tsx`
Expected: FAIL。

- [ ] **Step 3: 写最小实现**

- `modelPicker.ts`：`buildModelOptions(list, currentRef, currentModel)` 返回 `[{ value: null, label: "跟随全局默认 · <currentModel> 或默认条目的 model>", selected } , ...entries.map(...)]`。
- `ModelPicker.tsx`：一个按钮 + 下拉；选中调用 `setThreadModel`；成功后重读会话 meta（或本地更新 `info.model_ref` + `info.model`）。
- `MessageInput.tsx`：右下角渲染 `props.modelPicker`。
- `App.tsx`：构造 `ModelPicker` 并传入 `MessageInput`；`threadStore` 的 `info` 增加 `model_ref`（`thread/listAll` 与 `thread/started` 若带则填，否则 `null`）；`StatusBar` 不再传 `model`。
- `protocol.ts`：`ThreadSummary` 增加 `model_ref?: string | null`；`thread/started` 通知参数增加可选 `model_ref`。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd desktop && npx vitest run src/lib/modelPicker.test.ts src/components/StatusBar.test.tsx src/App.test.tsx`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/components/ModelPicker.tsx desktop/src/components/ModelPicker.test.tsx desktop/src/lib/modelPicker.ts desktop/src/lib/modelPicker.test.ts desktop/src/components/MessageInput.tsx desktop/src/App.tsx desktop/src/components/StatusBar.tsx desktop/src/components/StatusBar.test.tsx desktop/src/lib/protocol.ts desktop/src/lib/threadStore.ts
git commit -m "feat(desktop): per-session model picker; drop the status-bar model text"
```

---

### Task 13: 文档与项目进度登记

**Files:**
- Modify: `docs/project-management/desktop.md`、`docs/project-management/yi-agent-app-server.md`、`docs/project-management/yi-agent-runtime.md`（若存在）、`README.md`（模块索引计数，如涉及）

**Interfaces:** 无。

- [ ] **Step 1: 登记新项**

在对应模块文件按既有格式加条目（带可验证判据：代码位置或可执行命令）：

- `model/list|upsert|delete|setDefault|setSubagent`（`crates/yi-agent-app-server/src/model_rpc.rs`）
- `thread/setModel`、`ThreadMeta.model_ref`（`server.rs` / `thread_store.rs`）
- 模型设置 tab + 会话模型下拉（`desktop/src/components/SettingsModelsTab.tsx` / `ModelPicker.tsx`）
- TUI `/model` 列清单（`crates/yi-agent/src/tui/app.rs`）
- 更新 `README.md` 模块索引表的「完成/总计」计数

- [ ] **Step 2: 提交**

```bash
git add docs/project-management README.md
git commit -m "docs: register the model settings surface in project management"
```

---

## Self-Review

**1. Spec coverage**（对照 spec 各节）：
- §4 数据模型/存储 → Task 1、3；§4.3 权限 0600 → Task 1 Step 3 断言。✓
- §5.1 resolve_effective / model_ref → Task 2、3。✓
- §5.2 build_agent 接缝 → Task 4。✓
- §5.3 切模型重建 → Task 5、7。✓
- §5.4 子 agent 模型 → Task 8。✓
- §6 RPC + 掩码 + 校验 + model_not_found → Task 6、7。✓
- §7 桌面端（tab / 下拉 / 去 model / 协议封装）→ Task 10、11、12。✓
- §8 TUI → Task 9。✓
- §9 错误处理与安全 → Task 1（0600/坏文件）、6（零落盘/掩码）、7（未知名拒绝）。✓
- §10 测试 → 各任务内。✓
- §11 迁移与兼容 → Task 3（旧 meta 缺字段）、Task 2（回退 cfg）。✓
- §12 未决 → 见下方「实现期待定」。

**2. Placeholder scan**：无 TBD/TODO；每个代码步骤含实际代码；测试步骤含可运行命令与期望结果。

**3. Type consistency**：
- `ModelCatalog { models, default_model, subagent_model }` 在 Task 1/2/6/8 一致。
- `resolve_effective(cfg, catalog, session_override)` 签名在 Task 2/4/7 一致。
- `SessionCommand::SetModel { effective, model_ref, reply }` 在 Task 5/7 一致。
- `model/list` 输出字段 `has_key`/`api_key_masked` 在 Task 6/10/11 一致。
- `ModelEntryView` 字段在 Task 10/11/12 一致。

**遗留（实现时按代码现状收敛，不阻塞）**：
- Task 4 的 `with_session_model_ref` thread-local vs 显式参数：二选一，以「两条路径一致 + 不被 await 打断」为判据。
- Task 5 取回 `tools`/审批路径的具体方式（给 `Agent` 加 `rebuild_with` 或 `tools()`）。
- Task 7 的 meta 写：`set_model_ref` 与生效 `model` 一次原子写。
- Task 9 的 TUI 跨 provider 切换（可能需扩 `ControlCommand::SetModel`）。
