# WebUI Config Management Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add a `yi-agent web` subcommand that starts a WebUI for managing yi-agent's 14 environment variables via a .env file.

**Architecture:** New crate `yi-agent-web` with axum HTTP server + single embedded HTML page. The `yi-agent` bin gains clap subcommand support and `dotenvy` for .env loading at startup.

**Tech Stack:** axum 0.8, tokio, serde_json, dotenvy 0.15, clap subcommands, `include_str!` for HTML embedding.

---

## Task 1: Add workspace dependencies

**Files:**
- Modify: `yi-agent-rs/Cargo.toml`

**Step 1: Add axum and dotenvy to workspace deps**

Add to `[workspace.dependencies]` section after `crossbeam-channel`:

```toml
axum = "0.8"
dotenvy = "0.15"
```

**Step 2: Verify it compiles**

Run: `cd yi-agent-rs && cargo check`
Expected: no errors (deps not used yet, just declared)

**Step 3: Commit**

```bash
git add yi-agent-rs/Cargo.toml
git commit -m "chore: add axum and dotenvy workspace deps"
```

---

## Task 2: Add dotenvy to yi-agent crate and load .env

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/Cargo.toml`
- Modify: `yi-agent-rs/crates/yi-agent/src/config.rs`

**Step 1: Add dotenvy dependency to yi-agent crate**

In `yi-agent-rs/crates/yi-agent/Cargo.toml`, add to `[dependencies]`:

```toml
dotenvy = { workspace = true }
```

**Step 2: Write failing test for .env loading**

In `yi-agent-rs/crates/yi-agent/src/config.rs`, add this test to the `tests` module:

```rust
    #[test]
    fn load_reads_dotenv_file() {
        use std::io::Write;
        // 创建临时 .env 文件
        let temp_dir = std::env::temp_dir();
        let env_path = temp_dir.join(".env_test_dotenv_loading");
        let mut f = std::fs::File::create(&env_path).unwrap();
        writeln!(f, "MODEL_API_KEY=from-dotenv-file").unwrap();
        drop(f);

        // 加载 .env
        let _ = dotenvy::from_path(&env_path);

        let cli = Cli {
            provider: None,
            api_url: None,
            api_key: None,
            model: None,
            max_turns: None,
            workdir: Some(PathBuf::from(".")),
            system_prompt: None,
            compact_threshold: None,
            compact_keep_turns: None,
        };
        let config = load(&cli).unwrap();
        assert_eq!(config.api_key, "from-dotenv-file");

        // 清理
        unsafe { std::env::remove_var("MODEL_API_KEY"); }
        std::fs::remove_file(&env_path).ok();
    }
```

**Step 3: Run test to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent load_reads_dotenv_file`
Expected: FAIL — `dotenvy` not found or test fails

**Step 4: Add dotenvy loading to config::load()**

At the very top of the `load()` function in `config.rs` (before any `env::var` calls), add:

```rust
    // 从工作目录的 .env 文件加载环境变量（不覆盖已存在的）
    let env_path = cli
        .workdir
        .as_ref()
        .map(|w| w.join(".env"))
        .or_else(|| std::env::var("YI_AGENT_WORKDIR").ok().map(PathBuf::from).map(|p| p.join(".env")))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).join(".env"));
    let _ = dotenvy::from_path(&env_path);
```

**Step 5: Run test to verify it passes**

Run: `cd yi-agent-rs && cargo test -p yi-agent load_reads_dotenv_file`
Expected: PASS

**Step 6: Run all existing tests to verify no regression**

Run: `cd yi-agent-rs && cargo test -p yi-agent`
Expected: all tests PASS

**Step 7: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/Cargo.toml yi-agent-rs/crates/yi-agent/src/config.rs
git commit -m "feat(config): load .env file at startup via dotenvy"
```

---

## Task 3: Create yi-agent-web crate skeleton

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-web/Cargo.toml`
- Create: `yi-agent-rs/crates/yi-agent-web/src/lib.rs`
- Modify: `yi-agent-rs/Cargo.toml` (add to members)

**Step 1: Create Cargo.toml**

`yi-agent-rs/crates/yi-agent-web/Cargo.toml`:

```toml
[package]
name = "yi-agent-web"
description = "WebUI for yi-agent configuration"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true
authors.workspace = true

[dependencies]
tokio = { workspace = true }
anyhow = { workspace = true }
serde_json = { workspace = true }
axum = { workspace = true }

[dev-dependencies]
tempfile = "3"
```

**Step 2: Create lib.rs with minimal serve function**

`yi-agent-rs/crates/yi-agent-web/src/lib.rs`:

```rust
//! yi-agent WebUI：通过 Web 页面管理环境变量配置。

use std::path::PathBuf;

use anyhow::Result;

/// 启动 Web 配置服务器。
pub async fn serve(host: &str, port: u16, _env_path: PathBuf) -> Result<()> {
    let addr = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    let app = axum::Router::new();
    axum::serve(listener, app).await?;
    Ok(())
}
```

**Step 3: Add to workspace members**

In `yi-agent-rs/Cargo.toml`, add `"crates/yi-agent-web"` to the `members` array:

```toml
members = [
    "crates/yi-agent",
    "crates/yi-agent-core",
    "crates/yi-agent-llm",
    "crates/yi-agent-tools",
    "crates/yi-agent-mcp",
    "crates/yi-agent-store",
    "crates/yi-agent-web",
]
```

**Step 4: Verify it compiles**

Run: `cd yi-agent-rs && cargo check -p yi-agent-web`
Expected: compiles with no errors

**Step 5: Commit**

```bash
git add yi-agent-rs/Cargo.toml yi-agent-rs/crates/yi-agent-web/
git commit -m "feat(web): scaffold yi-agent-web crate"
```

---

## Task 4: Define variable metadata (config_meta module)

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-web/src/config_meta.rs`
- Modify: `yi-agent-rs/crates/yi-agent-web/src/lib.rs`

**Step 1: Write failing tests for metadata**

Create `yi-agent-rs/crates/yi-agent-web/src/config_meta.rs`:

```rust
//! 14 个环境变量的元数据定义。

/// 字段类型。
#[derive(Debug, Clone, PartialEq)]
pub enum VarType {
    Select,
    Secret,
    Text,
    Number,
    Path,
}

/// 单个环境变量的元数据。
#[derive(Debug, Clone)]
pub struct VarMeta {
    pub key: &'static str,
    pub default: Option<&'static str>,
    pub var_type: VarType,
    pub group: &'static str,
    pub description: &'static str,
    /// 仅 Select 类型使用
    pub options: &'static [&'static str],
}

/// 所有 14 个环境变量的元数据，按分组排列。
pub static ALL_VARS: &[VarMeta] = &[
    // === Provider ===
    VarMeta {
        key: "YI_AGENT_PROVIDER",
        default: Some("anthropic"),
        var_type: VarType::Select,
        group: "Provider",
        description: "LLM provider backend",
        options: &["anthropic", "openai"],
    },
    VarMeta {
        key: "MODEL_API_KEY",
        default: None,
        var_type: VarType::Secret,
        group: "Provider",
        description: "API key for the LLM provider",
        options: &[],
    },
    VarMeta {
        key: "MODEL_API_URL",
        default: None,
        var_type: VarType::Text,
        group: "Provider",
        description: "API endpoint URL override",
        options: &[],
    },
    VarMeta {
        key: "YI_AGENT_MODEL",
        default: None,
        var_type: VarType::Text,
        group: "Provider",
        description: "Model identifier string",
        options: &[],
    },
    // === Agent ===
    VarMeta {
        key: "YI_AGENT_MAX_TURNS",
        default: Some("20"),
        var_type: VarType::Number,
        group: "Agent",
        description: "Max agent turns per conversation",
        options: &[],
    },
    VarMeta {
        key: "YI_AGENT_WORKDIR",
        default: None,
        var_type: VarType::Path,
        group: "Agent",
        description: "Working directory for file tools",
        options: &[],
    },
    VarMeta {
        key: "YI_AGENT_SYSTEM_PROMPT",
        default: None,
        var_type: VarType::Text,
        group: "Agent",
        description: "Custom system prompt override",
        options: &[],
    },
    VarMeta {
        key: "YI_AGENT_COMPACT_THRESHOLD",
        default: Some("100000"),
        var_type: VarType::Number,
        group: "Agent",
        description: "Token threshold for auto-compact",
        options: &[],
    },
    VarMeta {
        key: "YI_AGENT_COMPACT_KEEP_TURNS",
        default: Some("4"),
        var_type: VarType::Number,
        group: "Agent",
        description: "Turns retained during compaction",
        options: &[],
    },
    // === Anthropic Provider ===
    VarMeta {
        key: "ANTHROPIC_API_KEY",
        default: None,
        var_type: VarType::Secret,
        group: "Anthropic Provider",
        description: "Anthropic provider API key",
        options: &[],
    },
    VarMeta {
        key: "ANTHROPIC_BASE_URL",
        default: Some("https://api.anthropic.com"),
        var_type: VarType::Text,
        group: "Anthropic Provider",
        description: "Anthropic API base URL",
        options: &[],
    },
    // === OpenAI Provider ===
    VarMeta {
        key: "OPENAI_API_KEY",
        default: None,
        var_type: VarType::Secret,
        group: "OpenAI Provider",
        description: "OpenAI provider API key",
        options: &[],
    },
    VarMeta {
        key: "OPENAI_BASE_URL",
        default: Some("https://api.openai.com"),
        var_type: VarType::Text,
        group: "OpenAI Provider",
        description: "OpenAI API base URL",
        options: &[],
    },
    // === Tools ===
    VarMeta {
        key: "BOCHA_API_KEY",
        default: None,
        var_type: VarType::Secret,
        group: "Tools",
        description: "Bocha web search API key",
        options: &[],
    },
];

/// 返回所有分组名称（按出现顺序，去重）。
pub fn groups() -> Vec<&'static str> {
    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();
    for var in ALL_VARS {
        if seen.insert(var.group) {
            result.push(var.group);
        }
    }
    result
}

/// 按 key 查找元数据。
pub fn find(key: &str) -> Option<&'static VarMeta> {
    ALL_VARS.iter().find(|v| v.key == key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_vars_count_is_14() {
        assert_eq!(ALL_VARS.len(), 14);
    }

    #[test]
    fn groups_are_ordered() {
        let g = groups();
        assert_eq!(g, vec!["Provider", "Agent", "Anthropic Provider", "OpenAI Provider", "Tools"]);
    }

    #[test]
    fn find_returns_meta() {
        let m = find("YI_AGENT_PROVIDER").unwrap();
        assert_eq!(m.var_type, VarType::Select);
        assert_eq!(m.options, &["anthropic", "openai"]);
    }

    #[test]
    fn find_returns_none_for_unknown() {
        assert!(find("UNKNOWN_VAR").is_none());
    }

    #[test]
    fn select_vars_have_options() {
        for var in ALL_VARS {
            if var.var_type == VarType::Select {
                assert!(!var.options.is_empty(), "{} is Select but has no options", var.key);
            }
        }
    }

    #[test]
    fn secret_vars_have_no_options() {
        for var in ALL_VARS {
            if var.var_type == VarType::Secret {
                assert!(var.options.is_empty(), "{} is Secret but has options", var.key);
            }
        }
    }

    #[test]
    fn all_keys_are_unique() {
        let mut keys: Vec<&str> = ALL_VARS.iter().map(|v| v.key).collect();
        keys.sort();
        let before = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), before, "duplicate keys found");
    }
}
```

**Step 2: Add module to lib.rs**

In `yi-agent-rs/crates/yi-agent-web/src/lib.rs`, add at the top:

```rust
pub mod config_meta;
```

**Step 3: Run tests to verify they pass**

Run: `cd yi-agent-rs && cargo test -p yi-agent-web`
Expected: all tests PASS (metadata is static, tests verify structure)

**Step 4: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-web/src/config_meta.rs yi-agent-rs/crates/yi-agent-web/src/lib.rs
git commit -m "feat(web): define 14 variable metadata with tests"
```

---

## Task 5: Implement .env file reader/writer (env_file module)

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-web/src/env_file.rs`
- Modify: `yi-agent-rs/crates/yi-agent-web/src/lib.rs`

**Step 1: Write failing tests**

Create `yi-agent-rs/crates/yi-agent-web/src/env_file.rs`:

```rust
//! .env 文件读写：解析 dotenv 格式，写入时保留分组注释。

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result, bail};

/// 解析 .env 文件，返回 key→value 映射。
/// 跳过注释行（以 # 开头）和空行。
pub fn read(path: &Path) -> Result<HashMap<String, String>> {
    let mut map = HashMap::new();
    if !path.exists() {
        return Ok(map);
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read .env file: {}", path.display()))?;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(eq_pos) = trimmed.find('=') {
            let key = trimmed[..eq_pos].trim().to_string();
            let value = trimmed[eq_pos + 1..].trim().to_string();
            // 去除可选的引号
            let value = strip_quotes(&value);
            if !key.is_empty() {
                map.insert(key, value);
            }
        }
    }
    Ok(map)
}

/// 去除值两端的引号（单引号或双引号）。
fn strip_quotes(s: &str) -> String {
    if s.len() >= 2 {
        let bytes = s.as_bytes();
        if (bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\'')
        {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

/// 将所有 14 个变量写入 .env 文件，带分组注释。
/// `current` 是当前已有的值，`updates` 是要覆盖的值。
pub fn write(
    path: &Path,
    current: &HashMap<String, String>,
    updates: &[(String, String)],
) -> Result<()> {
    use crate::config_meta::{ALL_VARS, VarType};

    // 合并 updates 到 current
    let mut merged = current.clone();
    for (key, value) in updates {
        merged.insert(key.clone(), value.clone());
    }

    let mut output = String::new();
    let mut last_group = "";

    for var in ALL_VARS {
        // 分组注释
        if var.group != last_group {
            if !last_group.is_empty() {
                output.push('\n');
            }
            output.push_str(&format!("# === {} ===\n", var.group));
            last_group = var.group;
        }

        let value = merged.get(var.key).map(|s| s.as_str()).unwrap_or("");
        output.push_str(&format!("{}={}\n", var.key, value));
    }

    std::fs::write(path, output)
        .with_context(|| format!("failed to write .env file: {}", path.display()))?;
    Ok(())
}

/// 对 secret 类型值做掩码：前4 + *** + 后4，不足12字符则全 ***。
pub fn mask(value: &str) -> String {
    if value.len() < 12 {
        return "***".to_string();
    }
    let bytes = value.as_bytes();
    let prefix = &value[..4];
    let suffix = &value[value.len() - 4..];
    format!("{}***{}", prefix, suffix)
}

/// 判断一个值是否是掩码值（包含 ***）。
pub fn is_masked(value: &str) -> bool {
    value.contains("***")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_meta::ALL_VARS;
    use tempfile::NamedTempFile;

    #[test]
    fn read_empty_file_returns_empty_map() {
        let tmp = NamedTempFile::new().unwrap();
        let map = read(tmp.path()).unwrap();
        assert!(map.is_empty());
    }

    #[test]
    fn read_nonexistent_file_returns_empty_map() {
        let map = read(Path::new("/nonexistent/path/.env")).unwrap();
        assert!(map.is_empty());
    }

    #[test]
    fn read_parses_key_value() {
        let tmp = NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "KEY=value\n").unwrap();
        let map = read(tmp.path()).unwrap();
        assert_eq!(map.get("KEY").unwrap(), "value");
    }

    #[test]
    fn read_skips_comments_and_empty_lines() {
        let tmp = NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "# comment\n\nKEY=value\n# another\n").unwrap();
        let map = read(tmp.path()).unwrap();
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("KEY").unwrap(), "value");
    }

    #[test]
    fn read_strips_quotes() {
        let tmp = NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "A=\"quoted\"\nB='single'\nC=plain\n").unwrap();
        let map = read(tmp.path()).unwrap();
        assert_eq!(map.get("A").unwrap(), "quoted");
        assert_eq!(map.get("B").unwrap(), "single");
        assert_eq!(map.get("C").unwrap(), "plain");
    }

    #[test]
    fn read_handles_values_with_equals() {
        let tmp = NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "URL=https://example.com?a=b&c=d\n").unwrap();
        let map = read(tmp.path()).unwrap();
        assert_eq!(map.get("URL").unwrap(), "https://example.com?a=b&c=d");
    }

    #[test]
    fn write_creates_file_with_all_vars() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        std::fs::remove_file(&path).ok(); // 删除临时文件，让 write 创建

        write(&path, &HashMap::new(), &[]).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        for var in ALL_VARS {
            assert!(content.contains(var.key), "missing {} in output", var.key);
        }
        assert!(content.contains("# === Provider ==="));
        assert!(content.contains("# === Agent ==="));
    }

    #[test]
    fn write_merges_updates() {
        let tmp = NamedTempFile::new().unwrap();
        let mut current = HashMap::new();
        current.insert("YI_AGENT_MODEL".to_string(), "old-model".to_string());

        write(tmp.path(), &current, &[("YI_AGENT_MODEL".to_string(), "new-model".to_string())]).unwrap();
        let map = read(tmp.path()).unwrap();
        assert_eq!(map.get("YI_AGENT_MODEL").unwrap(), "new-model");
    }

    #[test]
    fn write_preserves_unupdated_values() {
        let tmp = NamedTempFile::new().unwrap();
        let mut current = HashMap::new();
        current.insert("YI_AGENT_MODEL".to_string(), "keep-this".to_string());

        write(tmp.path(), &current, &[("YI_AGENT_MAX_TURNS".to_string(), "50".to_string())]).unwrap();
        let map = read(tmp.path()).unwrap();
        assert_eq!(map.get("YI_AGENT_MODEL").unwrap(), "keep-this");
        assert_eq!(map.get("YI_AGENT_MAX_TURNS").unwrap(), "50");
    }

    #[test]
    fn mask_long_value() {
        let m = mask("sk-ant-api03-xxxxxxxxxxxx");
        assert_eq!(m, "sk-a***xxxx");
    }

    #[test]
    fn mask_short_value() {
        assert_eq!(mask("short"), "***");
    }

    #[test]
    fn mask_exact_12_chars() {
        let m = mask("123456789012");
        assert_eq!(m, "1234***9012");
    }

    #[test]
    fn is_masked_detects_mask() {
        assert!(is_masked("sk-a***xxxx"));
        assert!(!is_masked("sk-ant-real-key"));
    }
}
```

**Step 2: Add module to lib.rs**

In `yi-agent-rs/crates/yi-agent-web/src/lib.rs`, add:

```rust
pub mod env_file;
```

**Step 3: Run tests to verify they pass**

Run: `cd yi-agent-rs && cargo test -p yi-agent-web`
Expected: all env_file tests PASS

**Step 4: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-web/src/env_file.rs yi-agent-rs/crates/yi-agent-web/src/lib.rs
git commit -m "feat(web): implement .env reader/writer with masking"
```

---

## Task 6: Implement HTTP API handlers (api module)

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-web/src/api.rs`
- Modify: `yi-agent-rs/crates/yi-agent-web/src/lib.rs`

**Step 1: Write api.rs with handlers**

Create `yi-agent-rs/crates/yi-agent-web/src/api.rs`:

```rust
//! HTTP API handlers for config read/write.

use std::path::PathBuf;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Json};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config_meta::{ALL_VARS, VarType, groups};
use crate::env_file;

/// 共享状态：.env 文件路径
#[derive(Clone)]
pub struct AppState {
    pub env_path: PathBuf,
}

/// GET / — 返回内嵌 HTML 页面
pub async fn index_html() -> Html<&'static str> {
    Html(include_str!("assets/index.html"))
}

/// GET /api/config — 返回所有变量元数据 + 当前值
pub async fn get_config(State(state): State<AppState>) -> impl IntoResponse {
    let vars = match env_file::read(&state.env_path) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("failed to read .env: {e}") })),
            );
        }
    };

    let mut group_list: Vec<Value> = Vec::new();
    for group_name in groups() {
        let mut var_list: Vec<Value> = Vec::new();
        for var in ALL_VARS.iter().filter(|v| v.group == group_name) {
            let raw_value = vars.get(var.key).cloned().unwrap_or_default();
            let (display_value, masked) = if var.var_type == VarType::Secret && !raw_value.is_empty() {
                (env_file::mask(&raw_value), true)
            } else {
                (raw_value.clone(), false)
            };
            var_list.push(json!({
                "key": var.key,
                "value": display_value,
                "default": var.default,
                "type": format!("{:?}", var.var_type).to_lowercase(),
                "description": var.description,
                "options": var.options,
                "masked": masked,
            }));
        }
        group_list.push(json!({
            "name": group_name,
            "vars": var_list,
        }));
    }

    Json(json!({
        "groups": group_list,
        "envPath": state.env_path.display().to_string(),
    }))
}

#[derive(Deserialize)]
pub struct PutConfigRequest {
    pub updates: Vec<(String, String)>,
}

/// PUT /api/config — 接收部分更新，写入 .env
pub async fn put_config(
    State(state): State<AppState>,
    Json(req): Json<PutConfigRequest>,
) -> impl IntoResponse {
    let current = match env_file::read(&state.env_path) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("failed to read .env: {e}") })),
            );
        }
    };

    // 过滤掉掩码值（secret 字段未修改时前端会发回掩码值）
    let mut filtered_updates: Vec<(String, String)> = Vec::new();
    for (key, value) in req.updates {
        if let Some(meta) = crate::config_meta::find(&key) {
            if meta.var_type == VarType::Secret && env_file::is_masked(&value) {
                // 掩码值跳过，不写入
                continue;
            }
        }
        filtered_updates.push((key, value));
    }

    match env_file::write(&state.env_path, &current, &filtered_updates) {
        Ok(()) => Json(json!({ "ok": true })),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("failed to write .env: {e}") })),
        ),
    }
}
```

**Step 2: Add serde dependency and module**

In `yi-agent-rs/crates/yi-agent-web/Cargo.toml`, add to `[dependencies]`:

```toml
serde = { version = "1", features = ["derive"] }
```

In `yi-agent-rs/crates/yi-agent-web/src/lib.rs`, add:

```rust
pub mod api;
```

**Step 3: Create placeholder HTML file**

Create `yi-agent-rs/crates/yi-agent-web/src/assets/index.html`:

```html
<!DOCTYPE html>
<html lang="zh">
<head><meta charset="UTF-8"><title>yi-agent 配置</title></head>
<body><p>Loading...</p></body>
</html>
```

**Step 4: Verify it compiles**

Run: `cd yi-agent-rs && cargo check -p yi-agent-web`
Expected: compiles with no errors

**Step 5: Write integration test for API**

Create `yi-agent-rs/crates/yi-agent-web/tests/api_test.rs`:

```rust
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use std::path::PathBuf;
use tempfile::TempDir;
use tower::ServiceExt;

use yi_agent_web::api::{AppState, get_config, index_html, put_config};
use yi_agent_web::config_meta::ALL_VARS;

/// 构建 axum app 用于测试
fn test_app(env_path: PathBuf) -> axum::Router {
    use axum::routing::{get, put};
    let state = AppState { env_path };
    axum::Router::new()
        .route("/", get(index_html))
        .route("/api/config", get(get_config).put(put_config))
        .with_state(state)
}

#[tokio::test]
async fn get_config_returns_all_groups() {
    let tmp = TempDir::new().unwrap();
    let env_path = tmp.path().join(".env");
    let app = test_app(env_path);

    let response = app
        .oneshot(Request::builder().uri("/api/config").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    let groups = json["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 5); // Provider, Agent, Anthropic, OpenAI, Tools

    // 验证包含所有 14 个变量
    let total_vars: usize = groups.iter().map(|g| g["vars"].as_array().unwrap().len()).sum();
    assert_eq!(total_vars, 14);
}

#[tokio::test]
async fn get_config_masks_secret_values() {
    let tmp = TempDir::new().unwrap();
    let env_path = tmp.path().join(".env");
    std::fs::write(&env_path, "MODEL_API_KEY=sk-ant-api03-xxxxxxxxxxxx\n").unwrap();
    let app = test_app(env_path);

    let response = app
        .oneshot(Request::builder().uri("/api/config").body(Body::empty()).unwrap())
        .await
        .unwrap();

    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    // 找到 MODEL_API_KEY
    for group in json["groups"].as_array().unwrap() {
        for var in group["vars"].as_array().unwrap() {
            if var["key"] == "MODEL_API_KEY" {
                assert_eq!(var["masked"], true);
                assert!(var["value"].as_str().unwrap().contains("***"));
                return;
            }
        }
    }
    panic!("MODEL_API_KEY not found in response");
}

#[tokio::test]
async fn put_config_writes_updates() {
    let tmp = TempDir::new().unwrap();
    let env_path = tmp.path().join(".env");
    let app = test_app(env_path.clone());

    let body = json!({
        "updates": [
            { "key": "YI_AGENT_MODEL", "value": "test-model-123" }
        ]
    });
    let request = Request::builder()
        .method("PUT")
        .uri("/api/config")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // 验证文件写入
    let content = std::fs::read_to_string(&env_path).unwrap();
    assert!(content.contains("YI_AGENT_MODEL=test-model-123"));
}

#[tokio::test]
async fn put_config_skips_masked_secrets() {
    let tmp = TempDir::new().unwrap();
    let env_path = tmp.path().join(".env");
    std::fs::write(&env_path, "MODEL_API_KEY=sk-ant-real-key-12345\n").unwrap();
    let app = test_app(env_path.clone());

    // 发送掩码值（应被跳过）
    let body = json!({
        "updates": [
            { "key": "MODEL_API_KEY", "value": "sk-a***2345" }
        ]
    });
    let request = Request::builder()
        .method("PUT")
        .uri("/api/config")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // 验证原值未被覆盖
    let map = yi_agent_web::env_file::read(&env_path).unwrap();
    assert_eq!(map.get("MODEL_API_KEY").unwrap(), "sk-ant-real-key-12345");
}

#[tokio::test]
async fn index_html_returns_html() {
    let tmp = TempDir::new().unwrap();
    let env_path = tmp.path().join(".env");
    let app = test_app(env_path);

    let response = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(html.contains("<html") || html.contains("<!DOCTYPE"));
}
```

**Step 6: Add tower dev-dependency for testing**

In `yi-agent-rs/crates/yi-agent-web/Cargo.toml`, add to `[dev-dependencies]`:

```toml
tower = { version = "0.5", features = ["util"] }
http-body-util = "0.1"
```

**Step 7: Run tests to verify they pass**

Run: `cd yi-agent-rs && cargo test -p yi-agent-web`
Expected: all tests PASS

**Step 8: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-web/
git commit -m "feat(web): implement HTTP API with integration tests"
```

---

## Task 7: Wire up serve() with router

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-web/src/lib.rs`

**Step 1: Update serve() to use real router**

Replace `yi-agent-rs/crates/yi-agent-web/src/lib.rs` with:

```rust
//! yi-agent WebUI：通过 Web 页面管理环境变量配置。

pub mod api;
pub mod config_meta;
pub mod env_file;

use std::path::PathBuf;

use anyhow::Result;
use axum::routing::{get, put};

/// 启动 Web 配置服务器。
pub async fn serve(host: &str, port: u16, env_path: PathBuf) -> Result<()> {
    let state = api::AppState { env_path };
    let app = axum::Router::new()
        .route("/", get(api::index_html))
        .route("/api/config", get(api::get_config).put(api::put_config))
        .with_state(state);

    let addr = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
```

**Step 2: Verify it compiles**

Run: `cd yi-agent-rs && cargo check -p yi-agent-web`
Expected: compiles with no errors

**Step 3: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-web/src/lib.rs
git commit -m "feat(web): wire up serve() with router"
```

---

## Task 8: Add `web` subcommand to yi-agent CLI

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/Cargo.toml`
- Modify: `yi-agent-rs/crates/yi-agent/src/config.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`

**Step 1: Add yi-agent-web dependency to yi-agent crate**

In `yi-agent-rs/crates/yi-agent/Cargo.toml`, add to `[dependencies]`:

```toml
yi-agent-web = { workspace = true }
```

In `yi-agent-rs/Cargo.toml` workspace deps, add:

```toml
yi-agent-web = { path = "crates/yi-agent-web" }
```

**Step 2: Add subcommand to Cli struct**

In `yi-agent-rs/crates/yi-agent/src/config.rs`, change the `Cli` struct to support subcommands:

```rust
/// clap CLI 参数定义。
#[derive(clap::Parser, Debug)]
#[command(name = "yi-agent", version, about = "Interactive AI agent CLI")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// LLM provider: "anthropic" or "openai" (overrides YI_AGENT_PROVIDER)
    #[arg(long)]
    pub provider: Option<String>,

    /// API endpoint URL (overrides MODEL_API_URL)
    #[arg(long)]
    pub api_url: Option<String>,

    /// API key (overrides MODEL_API_KEY)
    #[arg(long)]
    pub api_key: Option<String>,

    /// Model to use
    #[arg(long)]
    pub model: Option<String>,

    /// Max agent turns per conversation
    #[arg(long)]
    pub max_turns: Option<u32>,

    /// Working directory for file system tools
    #[arg(long)]
    pub workdir: Option<PathBuf>,

    /// Custom system prompt
    #[arg(long)]
    pub system_prompt: Option<String>,

    /// Token threshold for auto-compact
    #[arg(long)]
    pub compact_threshold: Option<u32>,

    /// Number of recent turns to keep during compact
    #[arg(long)]
    pub compact_keep_turns: Option<u32>,
}

/// 子命令
#[derive(clap::Subcommand, Debug)]
pub enum Command {
    /// Start web config UI
    Web {
        /// Host to bind
        #[arg(long, default_value = "127.0.0.1")]
        host: String,

        /// Port to bind
        #[arg(long, default_value = "7292")]
        port: u16,
    },
}
```

**Step 3: Update main.rs to handle subcommand**

Replace `yi-agent-rs/crates/yi-agent/src/main.rs` with:

```rust
//! yi-agent CLI 入口。

mod app;
mod compact;
mod config;
mod file_ref;
mod input;
mod render;

use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use render::InlineRenderer;
use yi_agent_core::Provider;

use crate::app::App;
use crate::config::{Cli, Command};

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::Web { host, port }) => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async {
                // 确定 .env 路径：优先 workdir CLI 参数，否则当前目录
                let workdir = cli
                    .workdir
                    .clone()
                    .or_else(|| std::env::var("YI_AGENT_WORKDIR").ok().map(std::path::PathBuf::from))
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")));
                let env_path = workdir.join(".env");
                yi_agent_web::serve(&host, port, env_path).await
            })
        }
        None => run_agent(cli),
    }
}

fn run_agent(cli: Cli) -> Result<()> {
    let config = config::load(&cli)?;

    let provider: Arc<dyn Provider> = match config.provider.as_str() {
        "anthropic" => Arc::new(yi_agent_llm::AnthropicProvider::new(
            yi_agent_llm::AnthropicProviderOpts {
                base_url: Some(config.api_url.clone()),
                api_key: Some(config.api_key.clone()),
                ..Default::default()
            },
        )?),
        "openai" => Arc::new(yi_agent_llm::OpenaiProvider::new(
            yi_agent_llm::OpenaiProviderOpts {
                base_url: Some(config.api_url.clone()),
                api_key: Some(config.api_key.clone()),
                ..Default::default()
            },
        )?),
        other => anyhow::bail!(
            "unknown provider '{}': expected 'anthropic' or 'openai'",
            other
        ),
    };

    let mut registry = yi_agent_core::ToolRegistry::new();
    yi_agent_tools::register_builtin_tools(&mut registry, config.workdir.clone());
    let tools = Arc::new(registry);

    let agent_config = yi_agent_core::AgentConfig {
        model: config.model.clone(),
        system_prompt: config.system_prompt.clone(),
        max_turns: Some(config.max_turns),
        compact_threshold: Some(config.compact_threshold),
        compact_keep_turns: Some(config.compact_keep_turns),
        ..Default::default()
    };

    let agent = yi_agent_core::Agent::new(
        Arc::clone(&provider),
        Arc::clone(&tools),
        agent_config.clone(),
    );

    let printer = reedline::ExternalPrinter::default();
    let renderer = Box::new(InlineRenderer::with_printer(printer.sender()));

    let app = App::new(
        agent,
        provider,
        tools,
        agent_config,
        config.workdir.clone(),
        renderer,
    );

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(app.run(printer))?;

    Ok(())
}
```

**Step 4: Update existing tests that construct Cli**

In `yi-agent-rs/crates/yi-agent/src/config.rs` tests, every `Cli { ... }` construction needs a `command: None` field. Add `command: None,` to each test's Cli literal. There are 7 test cases that need this update.

Also in `yi-agent-rs/crates/yi-agent/src/input.rs`, check for any Cli construction in tests.

**Step 5: Verify it compiles**

Run: `cd yi-agent-rs && cargo check -p yi-agent`
Expected: compiles with no errors

**Step 6: Run all tests**

Run: `cd yi-agent-rs && cargo test -p yi-agent`
Expected: all tests PASS

**Step 7: Commit**

```bash
git add yi-agent-rs/Cargo.toml yi-agent-rs/crates/yi-agent/Cargo.toml yi-agent-rs/crates/yi-agent/src/config.rs yi-agent-rs/crates/yi-agent/src/main.rs
git commit -m "feat(cli): add 'web' subcommand for config UI"
```

---

## Task 9: Build the frontend HTML page

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-web/src/assets/index.html`

**Step 1: Write the full HTML page**

Replace `yi-agent-rs/crates/yi-agent-web/src/assets/index.html` with:

```html
<!DOCTYPE html>
<html lang="zh">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>yi-agent 配置</title>
<style>
  :root {
    --bg: #1a1b26;
    --surface: #24283b;
    --surface-hover: #2f334d;
    --border: #3b4261;
    --text: #c0caf5;
    --text-dim: #565f89;
    --accent: #7aa2f7;
    --accent-hover: #89b4fa;
    --success: #9ece6a;
    --warning: #e0af68;
    --error: #f7768e;
    --radius: 8px;
  }
  * { margin: 0; padding: 0; box-sizing: border-box; }
  body {
    font-family: -apple-system, "Segoe UI", Roboto, sans-serif;
    background: var(--bg);
    color: var(--text);
    line-height: 1.6;
    padding: 2rem 1rem;
    max-width: 800px;
    margin: 0 auto;
  }
  h1 { font-size: 1.5rem; margin-bottom: 0.5rem; }
  .env-path {
    font-family: "SF Mono", "Fira Code", monospace;
    font-size: 0.85rem;
    color: var(--text-dim);
    margin-bottom: 2rem;
    padding: 0.5rem 0.75rem;
    background: var(--surface);
    border-radius: var(--radius);
    border: 1px solid var(--border);
    word-break: break-all;
  }
  .group {
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: var(--radius);
    padding: 1.25rem;
    margin-bottom: 1.5rem;
  }
  .group h2 {
    font-size: 1rem;
    color: var(--accent);
    margin-bottom: 1rem;
    padding-bottom: 0.5rem;
    border-bottom: 1px solid var(--border);
  }
  .field { margin-bottom: 1rem; }
  .field:last-child { margin-bottom: 0; }
  .field label {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    font-family: "SF Mono", "Fira Code", monospace;
    font-size: 0.85rem;
    color: var(--text);
    margin-bottom: 0.35rem;
  }
  .field .desc {
    font-family: -apple-system, sans-serif;
    font-size: 0.75rem;
    color: var(--text-dim);
    font-weight: normal;
  }
  .field input, .field select {
    width: 100%;
    padding: 0.5rem 0.75rem;
    background: var(--bg);
    border: 1px solid var(--border);
    border-radius: var(--radius);
    color: var(--text);
    font-family: "SF Mono", "Fira Code", monospace;
    font-size: 0.85rem;
    outline: none;
    transition: border-color 0.15s;
  }
  .field input:focus, .field select:focus {
    border-color: var(--accent);
  }
  .field input.modified, .field select.modified {
    border-color: var(--warning);
  }
  .secret-wrapper {
    display: flex;
    gap: 0.5rem;
    align-items: center;
  }
  .secret-wrapper input { flex: 1; }
  .toggle-btn {
    padding: 0.5rem 0.75rem;
    background: var(--bg);
    border: 1px solid var(--border);
    border-radius: var(--radius);
    color: var(--text-dim);
    cursor: pointer;
    font-size: 0.8rem;
    white-space: nowrap;
    transition: all 0.15s;
  }
  .toggle-btn:hover { color: var(--text); border-color: var(--accent); }
  .actions {
    position: sticky;
    bottom: 0;
    background: var(--bg);
    padding: 1rem 0;
    display: flex;
    align-items: center;
    gap: 1rem;
  }
  .save-btn {
    padding: 0.6rem 2rem;
    background: var(--accent);
    color: var(--bg);
    border: none;
    border-radius: var(--radius);
    font-size: 0.9rem;
    font-weight: 600;
    cursor: pointer;
    transition: all 0.15s;
  }
  .save-btn:hover:not(:disabled) { background: var(--accent-hover); }
  .save-btn:disabled { opacity: 0.4; cursor: not-allowed; }
  .status {
    font-size: 0.85rem;
    color: var(--text-dim);
  }
  .status.unsaved { color: var(--warning); }
  .status.saved { color: var(--success); }
  .toast {
    position: fixed;
    top: 1rem;
    right: 1rem;
    padding: 0.75rem 1.25rem;
    background: var(--success);
    color: var(--bg);
    border-radius: var(--radius);
    font-size: 0.85rem;
    font-weight: 600;
    opacity: 0;
    transform: translateY(-10px);
    transition: all 0.3s;
    pointer-events: none;
  }
  .toast.show { opacity: 1; transform: translateY(0); }
  .toast.error { background: var(--error); }
</style>
</head>
<body>

<h1>yi-agent 配置</h1>
<div class="env-path" id="envPath">加载中...</div>
<div id="groups"></div>

<div class="actions">
  <button class="save-btn" id="saveBtn" disabled>保存</button>
  <span class="status" id="status"></span>
</div>

<div class="toast" id="toast"></div>

<script>
  let originalValues = {};
  let currentValues = {};

  async function loadConfig() {
    const resp = await fetch('/api/config');
    const data = await resp.json();
    document.getElementById('envPath').textContent = data.envPath;

    const container = document.getElementById('groups');
    container.innerHTML = '';

    for (const group of data.groups) {
      const groupEl = document.createElement('div');
      groupEl.className = 'group';
      groupEl.innerHTML = `<h2>${group.name}</h2>`;

      for (const v of group.vars) {
        originalValues[v.key] = v.value;
        currentValues[v.key] = v.value;

        const field = document.createElement('div');
        field.className = 'field';

        const label = document.createElement('label');
        label.innerHTML = `${v.key} <span class="desc">${v.description}</span>`;

        let inputEl;
        if (v.type === 'select') {
          inputEl = document.createElement('select');
          for (const opt of v.options) {
            const o = document.createElement('option');
            o.value = opt;
            o.textContent = opt;
            if (opt === v.value) o.selected = true;
            inputEl.appendChild(o);
          }
        } else {
          inputEl = document.createElement('input');
          inputEl.type = v.type === 'secret' ? 'password' : (v.type === 'number' ? 'number' : 'text');
          inputEl.value = v.value;
          inputEl.placeholder = v.default || '(none)';
        }

        inputEl.dataset.key = v.key;
        inputEl.dataset.type = v.type;
        inputEl.dataset.masked = v.masked;
        inputEl.addEventListener('input', onFieldChange);
        inputEl.addEventListener('change', onFieldChange);

        if (v.type === 'secret') {
          const wrapper = document.createElement('div');
          wrapper.className = 'secret-wrapper';

          // 点击 secret 字段时清空掩码值以便编辑
          inputEl.addEventListener('focus', () => {
            if (inputEl.dataset.masked === 'true' && inputEl.value.includes('***')) {
              inputEl.value = '';
              inputEl.dataset.masked = 'false';
            }
          });

          const toggle = document.createElement('button');
          toggle.className = 'toggle-btn';
          toggle.textContent = '显示';
          toggle.type = 'button';
          toggle.addEventListener('click', () => {
            if (inputEl.type === 'password') {
              inputEl.type = 'text';
              toggle.textContent = '隐藏';
            } else {
              inputEl.type = 'password';
              toggle.textContent = '显示';
            }
          });

          wrapper.appendChild(inputEl);
          wrapper.appendChild(toggle);
          field.appendChild(wrapper);
        } else {
          field.appendChild(inputEl);
        }

        field.insertBefore(label, field.firstChild);
        groupEl.appendChild(field);
      }
      container.appendChild(groupEl);
    }
    updateSaveButton();
  }

  function onFieldChange(e) {
    const key = e.target.dataset.key;
    const newValue = e.target.value;
    currentValues[key] = newValue;

    const original = originalValues[key];
    if (newValue !== original) {
      e.target.classList.add('modified');
    } else {
      e.target.classList.remove('modified');
    }
    updateSaveButton();
  }

  function updateSaveButton() {
    let hasChanges = false;
    for (const key in currentValues) {
      if (currentValues[key] !== originalValues[key]) {
        hasChanges = true;
        break;
      }
    }
    const btn = document.getElementById('saveBtn');
    const status = document.getElementById('status');
    btn.disabled = !hasChanges;
    if (hasChanges) {
      status.className = 'status unsaved';
      status.textContent = '有未保存的修改';
    } else {
      status.className = 'status';
      status.textContent = '';
    }
  }

  async function save() {
    const updates = [];
    for (const key in currentValues) {
      if (currentValues[key] !== originalValues[key]) {
        updates.push({ key, value: currentValues[key] });
      }
    }
    if (updates.length === 0) return;

    const resp = await fetch('/api/config', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ updates }),
    });

    if (resp.ok) {
      showToast('已保存', false);
      await loadConfig();
    } else {
      const data = await resp.json().catch(() => ({}));
      showToast(data.error || '保存失败', true);
    }
  }

  function showToast(msg, isError) {
    const toast = document.getElementById('toast');
    toast.textContent = msg;
    toast.className = 'toast show' + (isError ? ' error' : '');
    setTimeout(() => toast.className = 'toast', 2500);
  }

  document.getElementById('saveBtn').addEventListener('click', save);
  loadConfig();
</script>

</body>
</html>
```

**Step 2: Verify it compiles (include_str! will pull it in)**

Run: `cd yi-agent-rs && cargo check -p yi-agent-web`
Expected: compiles with no errors

**Step 3: Run integration tests**

Run: `cd yi-agent-rs && cargo test -p yi-agent-web`
Expected: all tests PASS (index_html test checks for `<html` or `<!DOCTYPE`)

**Step 4: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-web/src/assets/index.html
git commit -m "feat(web): build frontend HTML page with dark theme"
```

---

## Task 10: Final verification

**Step 1: Build entire workspace**

Run: `cd yi-agent-rs && cargo build --workspace`
Expected: exit 0, no errors

**Step 2: Run all tests**

Run: `cd yi-agent-rs && cargo test --workspace`
Expected: all tests PASS, 0 failures

**Step 3: Run clippy**

Run: `cd yi-agent-rs && cargo clippy --workspace --all-targets`
Expected: no warnings on our code

**Step 4: Run fmt check**

Run: `cd yi-agent-rs && cargo fmt --all -- --check`
Expected: exit 0

**Step 5: Manual smoke test**

Run: `cd yi-agent-rs && cargo run -- web --port 7292`
Then open `http://127.0.0.1:7292` in a browser:
- Page should load with "yi-agent 配置" title
- All 5 groups should render with their fields
- .env path should be displayed at top
- Modify a text field, verify save button enables
- Click save, verify toast appears
- Check .env file was created/written in the workdir

**Step 6: Verify .env loading works**

Create a `.env` file in the worktree root with `MODEL_API_KEY=test-from-env`, then run:
`cd yi-agent-rs && cargo run -- --api-key dummy --workdir .`
The agent should start (using `test-from-env` as the API key, though it won't make real API calls).

**Step 7: Commit if any fixes were needed**

If clippy or fmt found issues, fix and commit:
```bash
git add -A
git commit -m "fix(web): address clippy/fmt issues"
```

---

## Summary

| Task | Description |
|------|-------------|
| 1 | Add axum + dotenvy workspace deps |
| 2 | Add dotenvy loading to config::load() |
| 3 | Scaffold yi-agent-web crate |
| 4 | Define 14 variable metadata |
| 5 | Implement .env reader/writer |
| 6 | Implement HTTP API + integration tests |
| 7 | Wire up serve() with router |
| 8 | Add `web` subcommand to CLI |
| 9 | Build frontend HTML page |
| 10 | Final verification |
