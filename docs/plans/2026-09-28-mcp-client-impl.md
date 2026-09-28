# MCP Client Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Implement a stdio-first MCP client in `yi-agent-mcp` that discovers remote tools from configured servers and exposes them to the agent as `mcp__{server}__{tool}` tools, with lazy connection and per-server enable switches.

**Architecture:** `McpManager` (Arc-shared) reads `.yi-agent/mcp.json`, loads tool schemas from a disk cache (`.yi-agent/mcp-cache.json`) or probes once to build it, and registers one `McpTool` adapter per discovered remote tool. Long-lived stdio connections via rmcp's `TokioChildProcess` are established lazily on first tool call. Session-level enable switches are toggled through the TUI slash command → control channel → agent driver.

**Tech Stack:** Rust 2024, `rmcp` 3.5 (client + `transport-child-process`), `tokio`, `serde`/`serde_json`, `async-trait`, `yi-agent-core` (`Tool` trait, `ToolRegistry`).

**Design doc:** `docs/plans/2026-09-28-mcp-client-design.md`

---

## Reference: rmcp 3.5.0 API (verified against docs.rs)

- Connect over stdio:
  ```rust
  use rmcp::{ServiceExt, transport::{TokioChildProcess, ConfigureCommandExt}};
  use tokio::process::Command;
  let transport = TokioChildProcess::new(
      Command::new("npx").configure(|c| { c.arg("-y").arg("@modelcontextprotocol/server-everything"); })
  )?;
  let client = ().serve(transport).await?;          // RunningService<RoleClient, ()>
  let tools = client.list_all_tools().await?;        // Vec<rmcp::model::Tool>
  let result = client.call_tool(
      rmcp::model::CallToolRequestParams::new("add")
  ).await?;                                          // CallToolResult
  ```
- `rmcp::model::Tool` fields: `name: Cow<'static,str>`, `title: Option<String>`,
  `description: Option<Cow<'static,str>>`, `input_schema: Arc<JsonObject>`,
  `annotations: Option<ToolAnnotations>`, ... `#[non_exhaustive]`.
  Use `tool.schema_as_json_value()` to get `serde_json::Value`.
- `rmcp::model::ToolAnnotations` fields (all `Option<bool>`): `read_only_hint`,
  `destructive_hint`, `idempotent_hint`, `open_world_hint`; plus `title`.
  `#[non_exhaustive]`. Read `read_only_hint` directly (`is_destructive` defaults to `true` when unset).
- `rmcp::model::CallToolRequestParams::new(name).with_arguments(JsonObject)`.
- `rmcp::model::CallToolResult` fields: `content: Vec<ContentBlock>`,
  `structured_content: Option<Value>`, `is_error: Option<bool>`.
- `rmcp::model::ContentBlock` variants (`#[non_exhaustive]`): `Text(TextContent)`,
  `Image(ImageContent)`, `Audio(AudioContent)`, `Resource(EmbeddedResource)`,
  `ResourceLink(Resource)`. Has `as_text()`, `as_image()`, `text(...)` ctors.
- `RunningService<RoleClient, ()>` has `call_tool`, `peer()`, `close()`,
  `close_with_timeout()`, `cancel()` (consuming), `waiting()`, and implements `Drop`.
- **Critical:** `TokioChildProcess::Drop` kills the child via a spawned tokio task —
  it only works if dropped **inside an active tokio runtime**. Do not rely on a drop
  after `std::process::exit`.

> If any signature above differs in the resolved `rmcp` version, re-check
> `https://docs.rs/rmcp/<version>/rmcp/` and adjust. The wrapper boundary in this plan
> isolates rmcp types to `manager.rs`.

---

## Task 0: Scaffold dependencies and modules

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-mcp/Cargo.toml`
- Modify: `yi-agent-rs/crates/yi-agent-mcp/src/lib.rs`

**Step 1: Add dependencies**

Replace the `[dependencies]` section of `crates/yi-agent-mcp/Cargo.toml` with:

```toml
[dependencies]
yi-agent-core = { workspace = true }
rmcp = { version = "3.5", default-features = false, features = ["client", "transport-child-process"] }
tokio = { workspace = true }
serde = { version = "1", features = ["derive"] }
serde_json = { workspace = true }
async-trait = "0.1"
anyhow = { workspace = true }
tracing = { workspace = true }

[dev-dependencies]
tempfile = "3"
rmcp = { version = "3.5", default-features = false, features = ["client", "server", "transport-child-process", "macros"] }
```

**Step 2: Declare module skeleton in `src/lib.rs`**

```rust
//! yi-agent-mcp: MCP (Model Context Protocol) client.
//!
//! 连接外部 MCP server,发现远端工具,通过实现 `yi-agent-core` 的 `Tool`
//! trait 把远端 MCP 工具接入 agent。

mod cache;
mod config;
mod manager;
mod naming;
mod tool;

pub use config::{McpConfig, ServerConfig};
pub use manager::McpManager;
pub use tool::McpTool;
```

Create the module files as empty stubs if needed so it compiles.

**Step 3: Verify it builds**

Run: `cd yi-agent-rs && cargo build -p yi-agent-mcp`
Expected: compiles (rmcp resolves). If rmcp features differ, adjust per docs.

**Step 4: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-mcp/
git commit -m "chore(mcp): add rmcp dependency and module skeleton"
```

---

## Task 1: Config parsing (`mcp.json`)

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-mcp/src/config.rs`

**Step 1: Write the failing tests**

Append to `config.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_config_with_defaults() {
        let raw = r#"{"mcpServers":{"fs":{"command":"npx","args":["-y","pkg"]}}}"#;
        let cfg: McpConfig = serde_json::from_str(raw).unwrap();
        assert!(cfg.enabled, "top-level enabled defaults to true");
        let fs = cfg.mcp_servers.get("fs").unwrap();
        assert_eq!(fs.command, "npx");
        assert_eq!(fs.args, vec!["-y", "pkg"]);
        assert!(fs.enabled, "per-server enabled defaults to true");
        assert!(fs.env.is_empty());
    }

    #[test]
    fn honors_explicit_false_flags() {
        let raw = r#"{"enabled":false,"mcpServers":{"git":{"command":"uvx","enabled":false}}}"#;
        let cfg: McpConfig = serde_json::from_str(raw).unwrap();
        assert!(!cfg.enabled);
        assert!(!cfg.mcp_servers.get("git").unwrap().enabled);
    }

    #[test]
    fn rejects_missing_command() {
        let raw = r#"{"mcpServers":{"bad":{"args":[]}}}"#;
        assert!(serde_json::from_str::<McpConfig>(raw).is_err());
    }

    #[test]
    fn load_returns_none_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert!(McpConfig::load(dir.path()).unwrap().is_none());
    }

    #[test]
    fn load_reads_file_from_dot_yi_agent() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join(".yi-agent");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("mcp.json"),
            r#"{"mcpServers":{"fs":{"command":"npx"}}}"#,
        )
        .unwrap();
        let cfg = McpConfig::load(dir.path()).unwrap().expect("some");
        assert!(cfg.mcp_servers.contains_key("fs"));
    }

    #[test]
    fn load_errors_on_invalid_json() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join(".yi-agent");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("mcp.json"), "{ not json").unwrap();
        assert!(McpConfig::load(dir.path()).is_err());
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cd yi-agent-rs && cargo test -p yi-agent-mcp --lib config`
Expected: FAIL (unresolved types / functions).

**Step 3: Write the implementation**

Top of `config.rs`:

```rust
//! `.yi-agent/mcp.json` configuration (Claude Desktop compatible schema).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Root of `.yi-agent/mcp.json`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct McpConfig {
    /// Master switch. Defaults to `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(rename = "mcpServers", default)]
    pub mcp_servers: BTreeMap<String, ServerConfig>,
}

/// One `mcpServers` entry.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// Path to the config file for a given working directory.
pub fn config_path(workdir: &Path) -> PathBuf {
    workdir.join(".yi-agent").join("mcp.json")
}

impl McpConfig {
    /// Load `.yi-agent/mcp.json` under `workdir`.
    ///
    /// Returns `Ok(None)` when the file does not exist (MCP not configured).
    pub fn load(workdir: &Path) -> Result<Option<Self>> {
        let path = config_path(workdir);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        let cfg = serde_json::from_str(&text)
            .with_context(|| format!("parse {}", path.display()))?;
        Ok(Some(cfg))
    }
}
```

**Step 4: Run tests to verify they pass**

Run: `cd yi-agent-rs && cargo test -p yi-agent-mcp --lib config`
Expected: PASS.

**Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-mcp/
git commit -m "feat(mcp): parse .yi-agent/mcp.json config"
```

---

## Task 2: Tool naming and sanitization

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-mcp/src/naming.rs`

**Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualified_name_uses_double_underscore() {
        assert_eq!(qualified_name("filesystem", "read_file"), "mcp__filesystem__read_file");
    }

    #[test]
    fn sanitizes_invalid_chars() {
        assert_eq!(qualified_name("my server", "read/file"), "mcp__my_server__read_file");
    }

    #[test]
    fn truncates_to_64_bytes() {
        let long = "a".repeat(200);
        let q = qualified_name(&long, &long);
        assert!(q.len() <= 64, "got {} bytes", q.len());
        assert!(q.starts_with("mcp__"));
    }

    #[test]
    fn sanitized_name_is_valid_tool_name_charset() {
        let q = qualified_name("srv..x", "weird*name");
        for ch in q.chars() {
            assert!(ch.is_ascii_alphanumeric() || ch == '_' || ch == '-', "bad char {ch:?}");
        }
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cd yi-agent-rs && cargo test -p yi-agent-mcp --lib naming`
Expected: FAIL.

**Step 3: Write the implementation**

```rust
//! Qualified naming for MCP tools exposed to the LLM.
//!
//! Provider tool-name limits (Anthropic): `^[a-zA-Z0-9_-]{1,64}$`.

use std::borrow::Cow;

const PREFIX: &str = "mcp__";
const MAX_LEN: usize = 64;

fn sanitize(part: &str) -> String {
    part.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect()
}

/// Build `mcp__{server}__{tool}`, sanitized to the provider charset and
/// truncated to 64 bytes (UTF-8 safe: only ASCII is produced).
pub fn qualified_name(server: &str, tool: &str) -> String {
    let base = format!("{PREFIX}{}__{}", sanitize(server), sanitize(tool));
    if base.len() <= MAX_LEN {
        return base;
    }
    // Truncate on an ASCII boundary (all bytes are ASCII here).
    base[..MAX_LEN].to_string()
}

// `Cow` import kept for future non-allocating paths; remove if unused.
#[allow(dead_code)]
fn _assert_cow<'a>(s: &'a str) -> Cow<'a, str> {
    Cow::Borrowed(s)
}
```

(Remove the `Cow` helper if the compiler warns; it exists only to document intent.
Simpler: delete the `use std::borrow::Cow;` and the helper outright.)

**Step 4: Run tests to verify they pass**

Run: `cd yi-agent-rs && cargo test -p yi-agent-mcp --lib naming`
Expected: PASS.

**Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-mcp/
git commit -m "feat(mcp): qualified tool naming with sanitization"
```

---

## Task 3: Annotation → metadata mapping

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-mcp/src/tool.rs` (start this file here)

**Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod meta_tests {
    use super::metadata_from_annotations;
    use yi_agent_core::ToolSource;

    #[test]
    fn read_only_hint_true_makes_tool_read_only_and_no_confirmation() {
        let meta = metadata_from_annotations("fs", Some(true));
        assert!(meta.read_only);
        assert!(!meta.requires_confirmation);
        assert_eq!(meta.source, ToolSource::Mcp { server_name: "fs".into() });
    }

    #[test]
    fn read_only_hint_false_requires_confirmation() {
        let meta = metadata_from_annotations("fs", Some(false));
        assert!(!meta.read_only);
        assert!(meta.requires_confirmation);
    }

    #[test]
    fn absent_annotations_default_to_confirming() {
        let meta = metadata_from_annotations("fs", None);
        assert!(!meta.read_only);
        assert!(meta.requires_confirmation);
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cd yi-agent-rs && cargo test -p yi-agent-mcp --lib meta_tests`
Expected: FAIL.

**Step 3: Write the implementation**

At the top of `tool.rs` (before the tests):

```rust
//! `McpTool`: adapts a remote MCP tool to `yi_agent_core::Tool`.

use yi_agent_core::{ToolMetadata, ToolSource};

/// Map MCP `annotations.readOnlyHint` to `ToolMetadata`.
///
/// Anything not explicitly read-only requires confirmation.
pub(crate) fn metadata_from_annotations(server: &str, read_only_hint: Option<bool>) -> ToolMetadata {
    let read_only = read_only_hint.unwrap_or(false);
    ToolMetadata {
        source: ToolSource::Mcp { server_name: server.to_string() },
        requires_confirmation: !read_only,
        read_only,
        version: None,
    }
}
```

**Step 4: Run tests to verify they pass**

Run: `cd yi-agent-rs && cargo test -p yi-agent-mcp --lib meta_tests`
Expected: PASS.

**Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-mcp/
git commit -m "feat(mcp): map MCP annotations to tool metadata"
```

---

## Task 4: Schema cache with fingerprint invalidation

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-mcp/src/cache.rs`

**Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn server(cmd: &str) -> ServerConfig {
        ServerConfig {
            command: cmd.into(),
            args: vec!["--x".into()],
            env: BTreeMap::new(),
            enabled: true,
        }
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = McpCache::default();
        cache.put("fs", server("npx"), vec![CachedTool {
            name: "read".into(),
            description: Some("reads".into()),
            input_schema: serde_json::json!({"type":"object"}),
            read_only: true,
        }]);
        cache.save(dir.path()).unwrap();

        let loaded = McpCache::load(dir.path());
        let hit = loaded.get_for("fs", &server("npx"));
        assert_eq!(hit.map(|v| v.len()), Some(1));
        assert_eq!(hit.unwrap()[0].name, "read");
    }

    #[test]
    fn fingerprint_mismatch_invalidates() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = McpCache::default();
        cache.put("fs", server("npx"), vec![]);
        cache.save(dir.path()).unwrap();
        let loaded = McpCache::load(dir.path());
        assert!(loaded.get_for("fs", &server("uvx")).is_none());
    }

    #[test]
    fn missing_file_yields_empty_cache() {
        let dir = tempfile::tempdir().unwrap();
        let c = McpCache::load(dir.path());
        assert!(c.get_for("fs", &server("npx")).is_none());
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cd yi-agent-rs && cargo test -p yi-agent-mcp --lib cache`
Expected: FAIL.

**Step 3: Write the implementation**

```rust
//! On-disk tool-schema cache: `.yi-agent/mcp-cache.json`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::ServerConfig;

/// One cached remote tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedTool {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
    pub read_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedServer {
    /// JSON fingerprint of the `ServerConfig` (command+args+env).
    fingerprint: String,
    tools: Vec<CachedTool>,
}

/// Whole cache file.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct McpCache {
    #[serde(default)]
    servers: BTreeMap<String, CachedServer>,
}

fn cache_path(workdir: &Path) -> PathBuf {
    workdir.join(".yi-agent").join("mcp-cache.json")
}

fn fingerprint(cfg: &ServerConfig) -> String {
    // Deterministic: serialize the config (command+args+env; `enabled` is a
    // runtime switch and must not invalidate the schema cache).
    let key = serde_json::json!({
        "command": cfg.command,
        "args": cfg.args,
        "env": cfg.env,
    });
    serde_json::to_string(&key).unwrap_or_default()
}

impl McpCache {
    /// Load the cache; a missing or corrupt file yields an empty cache.
    pub fn load(workdir: &Path) -> Self {
        std::fs::read_to_string(cache_path(workdir))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    /// Return cached tools for `name` iff the config fingerprint matches.
    pub fn get_for(&self, name: &str, cfg: &ServerConfig) -> Option<&[CachedTool]> {
        let entry = self.servers.get(name)?;
        (entry.fingerprint == fingerprint(cfg)).then_some(entry.tools.as_slice())
    }

    pub fn put(&mut self, name: &str, cfg: ServerConfig, tools: Vec<CachedTool>) {
        self.servers.insert(
            name.to_string(),
            CachedServer { fingerprint: fingerprint(&cfg), tools },
        );
    }

    /// Persist the cache; best-effort (a failed write just logs).
    pub fn save(&self, workdir: &Path) -> Result<()> {
        let path = cache_path(workdir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("mkdir {}", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }
}
```

**Step 4: Run tests to verify they pass**

Run: `cd yi-agent-rs && cargo test -p yi-agent-mcp --lib cache`
Expected: PASS.

**Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-mcp/
git commit -m "feat(mcp): schema cache with config fingerprint"
```

---

## Task 5: `ToolRegistry::remove` (core)

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/tool.rs` (add method near `register`, ~line 140)
- Test: same file's `mod tests`

**Step 1: Write the failing test**

Add to `yi-agent-core/src/tool.rs` tests:

```rust
    #[test]
    fn registry_remove_by_name() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(EchoTool));
        assert!(reg.remove("echo").is_some());
        assert!(reg.get("echo").is_none());
        assert!(reg.remove("echo").is_none());
    }
```

**Step 2: Run test to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib registry_remove_by_name`
Expected: FAIL (`remove` not found).

**Step 3: Write the implementation**

Add after `register` in `impl ToolRegistry`:

```rust
    /// Remove a tool by name, returning it if present.
    pub fn remove(&mut self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.remove(name)
    }

    /// Names of all registered tools.
    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }
```

**Step 4: Run test to verify it passes**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib registry_remove_by_name`
Expected: PASS.

**Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-core/src/tool.rs
git commit -m "feat(core): add ToolRegistry::remove and names"
```

---

## Task 6: `McpTool` adapter (schema-only, no connection)

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-mcp/src/tool.rs`

The adapter holds everything needed to advertise a tool and delegates `call` to
the manager. At this task it compiles and its metadata/schema are correct; the
manager is introduced in Task 7.

**Step 1: Write the failing test**

Add to `tool.rs`:

```rust
#[cfg(test)]
mod adapter_tests {
    use super::*;

    #[test]
    fn exposes_qualified_name_description_and_schema() {
        let tool = McpTool::new_for_test(
            "fs",
            "read_file",
            "Reads a file",
            serde_json::json!({"type":"object","properties":{"path":{"type":"string"}}}),
            true,
        );
        assert_eq!(tool.name(), "mcp__fs__read_file");
        assert_eq!(tool.description(), "Reads a file");
        assert_eq!(tool.schema()["type"], "object");
        let meta = tool.metadata();
        assert!(meta.read_only);
        assert!(!meta.requires_confirmation);
    }
}
```

**Step 2: Run test to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent-mcp --lib adapter_tests`
Expected: FAIL (`McpTool` missing).

**Step 3: Write the implementation**

Add to `tool.rs`:

```rust
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use yi_agent_core::{Tool, ToolResult};

use crate::manager::McpManager;
use crate::naming::qualified_name;

/// Adapter exposing one remote MCP tool as a `yi_agent_core::Tool`.
pub struct McpTool {
    server: String,
    remote_name: String,
    qualified: String,
    description: String,
    input_schema: Value,
    read_only: bool,
    manager: Arc<McpManager>,
}

impl McpTool {
    pub(crate) fn new(
        server: String,
        remote_name: String,
        description: String,
        input_schema: Value,
        read_only: bool,
        manager: Arc<McpManager>,
    ) -> Self {
        let qualified = qualified_name(&server, &remote_name);
        Self { server, remote_name, qualified, description, input_schema, read_only, manager }
    }

    /// Construct without a manager — used by unit tests for name/metadata.
    #[cfg(test)]
    pub(crate) fn new_for_test(
        server: &str,
        remote_name: &str,
        description: &str,
        input_schema: Value,
        read_only: bool,
    ) -> Self {
        let qualified = qualified_name(server, remote_name);
        // A manager is required by the struct; tests never call `call`.
        let manager = McpManager::empty_for_test();
        Self {
            server: server.into(),
            remote_name: remote_name.into(),
            qualified,
            description: description.into(),
            input_schema,
            read_only,
            manager,
        }
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.qualified
    }

    fn schema(&self) -> Value {
        self.input_schema.clone()
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn metadata(&self) -> ToolMetadata {
        metadata_from_annotations(&self.server, Some(self.read_only))
    }

    async fn call(&self, args: Value) -> ToolResult {
        self.manager.call_tool(&self.server, &self.remote_name, args).await
    }
}
```

`read_only` is stored from the cached annotation. `metadata_from_annotations`
takes `Some(self.read_only)` — but note: `read_only=false` from a *missing*
annotation and from an explicit `false` are indistinguishable here; both require
confirmation, which is the intended default. Keep the cached `read_only` bool as
the single source of truth.

**Step 4: Run test to verify it passes**

Run: `cd yi-agent-rs && cargo test -p yi-agent-mcp --lib adapter_tests`
Expected: PASS.

**Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-mcp/
git commit -m "feat(mcp): McpTool adapter"
```

---

## Task 7: `McpManager` with lazy connection and `call_tool`

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-mcp/src/manager.rs`

**Step 1: Write the failing tests (connection is mocked via an injectable connector)**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::CachedTool;

    fn cfg(cmd: &str) -> ServerConfig {
        ServerConfig { command: cmd.into(), args: vec![], env: Default::default(), enabled: true }
    }

    #[test]
    fn effective_state_combines_master_and_server() {
        let mut m = McpManager::new_for_test(true);
        m.insert_server_for_test("fs", cfg("npx"), true);
        assert!(m.is_effective("fs"));
        m.set_master(false);
        assert!(!m.is_effective("fs"));
    }

    #[test]
    fn enabled_tools_skip_disabled_servers() {
        let mut m = McpManager::new_for_test(true);
        m.insert_server_with_tools_for_test("fs", cfg("npx"), true, vec![CachedTool {
            name: "read".into(), description: None,
            input_schema: serde_json::json!({"type":"object"}), read_only: true,
        }]);
        assert_eq!(m.enabled_tools().len(), 1);
        m.set_server("fs", false).unwrap();
        assert_eq!(m.enabled_tools().len(), 0);
    }

    #[test]
    fn call_on_disabled_server_returns_error_result() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut m = McpManager::new_for_test(true);
        m.insert_server_with_tools_for_test("fs", cfg("npx"), false, vec![]);
        let res = rt.block_on(m.call_tool("fs", "read", serde_json::json!({})));
        assert!(res.is_error);
    }

    #[test]
    fn call_unknown_server_returns_error_result() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let m = McpManager::new_for_test(true);
        let res = rt.block_on(m.call_tool("nope", "x", serde_json::json!({})));
        assert!(res.is_error);
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cd yi-agent-rs && cargo test -p yi-agent-mcp --lib manager`
Expected: FAIL.

**Step 3: Write the implementation**

```rust
//! `McpManager`: owns configured servers, lazily connects, and dispatches calls.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tracing::{info, warn};
use yi_agent_core::{Tool, ToolResult};

use crate::cache::CachedTool;
use crate::config::ServerConfig;
use crate::tool::McpTool;

/// Timeout for a single tool call.
const CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// rmcp client handle, transport erased.
type Client = rmcp::service::RunningService<rmcp::RoleClient, ()>;

/// Result of a remote call, pre-mapping.
struct ServerEntry {
    config: ServerConfig,
    /// Session-level per-server switch (overrides the file default).
    enabled: AtomicBool,
    tools: Vec<CachedTool>,
    client: Mutex<Option<Client>>,
}

/// Shared MCP server manager.
pub struct McpManager {
    servers: HashMap<String, ServerEntry>,
    /// Session-level master switch (overrides the file default).
    master_enabled: AtomicBool,
}

impl McpManager {
    /// Empty manager (test helper / no-config case).
    pub fn empty() -> Arc<Self> {
        Arc::new(Self { servers: HashMap::new(), master_enabled: AtomicBool::new(false) })
    }

    /// Effective state: master AND per-server.
    pub fn is_effective(&self, server: &str) -> bool {
        self.master_enabled.load(Ordering::Relaxed)
            && self.servers.get(server).map(|e| e.enabled.load(Ordering::Relaxed)).unwrap_or(false)
    }

    /// Set the session master switch.
    pub fn set_master(&self, on: bool) {
        self.master_enabled.store(on, Ordering::Relaxed);
    }

    pub fn master(&self) -> bool {
        self.master_enabled.load(Ordering::Relaxed)
    }

    /// Set a single server's session switch.
    pub fn set_server(&self, server: &str, on: bool) -> Result<()> {
        let entry = self.servers.get(server).ok_or_else(|| anyhow!("unknown MCP server: {server}"))?;
        entry.enabled.store(on, Ordering::Relaxed);
        Ok(())
    }

    /// Server names configured, in stable order.
    pub fn server_names(&self) -> Vec<String> {
        let mut v: Vec<_> = self.servers.keys().cloned().collect();
        v.sort();
        v
    }

    /// `(name, effective_on)` for display.
    pub fn status(&self) -> Vec<(String, bool)> {
        self.server_names().into_iter().map(|n| { let on = self.is_effective(&n); (n, on) }).collect()
    }

    /// All tool names this manager could register (used to unregister on toggle).
    pub fn all_tool_names(&self) -> Vec<String> {
        self.servers.values().flat_map(|e| {
            e.tools.iter().map(|t| crate::naming::qualified_name(&e.config.command, &t.name))
        }).collect()
    }

    /// `McpTool` instances for every effective-on server.
    pub fn enabled_tools(self: &Arc<Self>) -> Vec<Arc<dyn Tool>> {
        let mut out: Vec<Arc<dyn Tool>> = Vec::new();
        for (name, entry) in &self.servers {
            if !self.is_effective(name) {
                continue;
            }
            for t in &entry.tools {
                out.push(Arc::new(McpTool::new(
                    name.clone(),
                    t.name.clone(),
                    t.description.clone().unwrap_or_default(),
                    t.input_schema.clone(),
                    t.read_only,
                    Arc::clone(self),
                )));
            }
        }
        out
    }

    /// Execute a remote tool call, connecting lazily.
    pub async fn call_tool(&self, server: &str, remote_name: &str, args: Value) -> ToolResult {
        let Some(entry) = self.servers.get(server) else {
            return ToolResult::error(format!("unknown MCP server: {server}"));
        };
        if !self.is_effective(server) {
            return ToolResult::error(format!("MCP server '{server}' is disabled"));
        }

        let mut guard = entry.client.lock().await;
        if guard.is_none() {
            match connect(&entry.config).await {
                Ok(c) => {
                    info!(server, "connected to MCP server");
                    *guard = Some(c);
                }
                Err(e) => return ToolResult::error(format!("connect to '{server}' failed: {e}")),
            }
        }
        let client = guard.as_ref().expect("just connected");

        let params = rmcp::model::CallToolRequestParams::new(remote_name.to_string())
            .with_arguments(args.as_object().cloned().unwrap_or_default());

        match tokio::time::timeout(CALL_TIMEOUT, client.call_tool(params)).await {
            Ok(Ok(result)) => map_result(result),
            Ok(Err(e)) => ToolResult::error(format!("MCP call failed: {e}")),
            Err(_) => ToolResult::error(format!("MCP call timed out after {CALL_TIMEOUT:?}")),
        }
    }

    /// Drop all live connections. Call inside a tokio runtime.
    pub async fn shutdown(&self) {
        for (name, entry) in &self.servers {
            let mut guard = entry.client.lock().await;
            if let Some(client) = guard.take() {
                // `close` asks the service loop to stop; dropping the child
                // transport then kills the process (requires active runtime).
                let _ = client.cancel().await;
                info!(server = name, "MCP server connection closed");
            }
        }
    }

    // ---- test helpers ----
    #[cfg(test)]
    pub(crate) fn new_for_test(master: bool) -> Arc<Self> {
        Arc::new(Self { servers: HashMap::new(), master_enabled: AtomicBool::new(master) })
    }

    #[cfg(test)]
    pub(crate) fn empty_for_test() -> Arc<Self> {
        Self::new_for_test(false)
    }

    #[cfg(test)]
    pub(crate) fn insert_server_for_test(&mut self, name: &str, cfg: ServerConfig, on: bool) {
        self.servers.entry(name.to_string()).or_insert_with(|| ServerEntry {
            config: cfg, enabled: AtomicBool::new(on), tools: vec![], client: Mutex::new(None),
        });
    }

    #[cfg(test)]
    pub(crate) fn insert_server_with_tools_for_test(
        &mut self, name: &str, cfg: ServerConfig, on: bool, tools: Vec<CachedTool>,
    ) {
        self.servers.insert(name.to_string(), ServerEntry {
            config: cfg, enabled: AtomicBool::new(on), tools, client: Mutex::new(None),
        });
    }
}

/// Open a stdio connection to one server.
async fn connect(cfg: &ServerConfig) -> Result<Client> {
    use rmcp::ServiceExt;
    use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};

    let mut cmd = tokio::process::Command::new(&cfg.command);
    cmd.args(&cfg.args);
    for (k, v) in &cfg.env {
        cmd.env(k, v);
    }
    let transport = TokioChildProcess::new(cmd.configure(|_| {}))?;
    let client = ().serve(transport).await?;
    Ok(client)
}

/// Map an rmcp `CallToolResult` to `yi_agent_core::ToolResult`.
fn map_result(result: rmcp::model::CallToolResult) -> ToolResult {
    use yi_agent_core::message::{ContentBlock, ImageSource};

    let is_error = result.is_error.unwrap_or(false);
    let mut blocks: Vec<ContentBlock> = Vec::new();
    for block in &result.content {
        match block {
            rmcp::model::ContentBlock::Text(t) => {
                blocks.push(ContentBlock::Text(t.text.clone()));
            }
            rmcp::model::ContentBlock::Image(img) => {
                blocks.push(ContentBlock::Image {
                    source: ImageSource::Base64 {
                        media_type: img.mime_type.clone(),
                        data: img.data.clone(),
                    },
                    detail: Default::default(),
                });
            }
            other => {
                // Resource / audio / unknown: fall back to JSON text.
                blocks.push(ContentBlock::Text(
                    serde_json::to_string(other).unwrap_or_else(|_| "<unrepresentable>".into()),
                ));
            }
        }
    }
    if blocks.is_empty() {
        if let Some(sc) = &result.structured_content {
            blocks.push(ContentBlock::Text(sc.to_string()));
        }
    }
    if blocks.is_empty() {
        blocks.push(ContentBlock::Text(String::new()));
    }
    ToolResult { content: blocks, is_error }
}
```

Notes:
- `meta` is unused; delete it.
- `json!` import may be unused; remove if so.
- `configure(|_| {})` is a no-op because args/env were already applied to `cmd`.
  If `ConfigureCommandExt::configure` is not needed, drop it and construct the
  command directly (verify against the resolved rmcp version).
- `client.cancel()` consumes `client`; it is `async` and awaits cleanup.

**Step 4: Run tests to verify they pass**

Run: `cd yi-agent-rs && cargo test -p yi-agent-mcp --lib manager`
Expected: PASS.

**Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-mcp/
git commit -m "feat(mcp): McpManager with lazy connect and call mapping"
```

---

## Task 8: `register_mcp_tools` — startup load, cache probe, registration

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-mcp/src/lib.rs`

**Step 1: Write the failing tests**

Create `yi-agent-rs/crates/yi-agent-mcp/tests/registration.rs`:

```rust
use std::path::Path;
use yi_agent_core::ToolRegistry;

#[test]
fn no_config_registers_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut reg = ToolRegistry::new();
    let mgr = yi_agent_mcp::register_mcp_tools(&mut reg, dir.path()).unwrap();
    assert!(mgr.is_none());
    assert!(reg.is_empty());
}

#[test]
fn config_with_no_cache_and_bad_command_still_succeeds_without_tools() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().join(".yi-agent");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("mcp.json"),
        r#"{"mcpServers":{"broken":{"command":"/nonexistent/definitely-not-here"}}}"#,
    )
    .unwrap();

    let mut reg = ToolRegistry::new();
    let mgr = yi_agent_mcp::register_mcp_tools(&mut reg, dir.path()).unwrap();
    assert!(mgr.is_some(), "manager exists even if the server fails to probe");
    assert!(reg.is_empty(), "a failed probe must not register tools or panic");
}
```

**Step 2: Run tests to verify they fail**

Run: `cd yi-agent-rs && cargo test -p yi-agent-mcp --test registration`
Expected: FAIL.

**Step 3: Write the implementation**

Replace `src/lib.rs` body with:

```rust
mod cache;
mod config;
mod manager;
mod naming;
mod tool;

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use tracing::warn;
use yi_agent_core::ToolRegistry;

pub use cache::CachedTool;
pub use config::{McpConfig, ServerConfig};
pub use manager::McpManager;
pub use tool::McpTool;

/// Load MCP config, build the manager, probe missing caches, and register tools.
///
/// Returns `Ok(None)` when there is no `.yi-agent/mcp.json`.
/// Never fails the caller on a single bad server: it warns and continues.
pub fn register_mcp_tools(
    registry: &mut ToolRegistry,
    workdir: &Path,
) -> Result<Option<Arc<McpManager>>> {
    let Some(cfg) = McpConfig::load(workdir)? else {
        return Ok(None);
    };

    let rt = tokio::runtime::Handle::try_current();
    let manager = match rt {
        Ok(handle) => handle.block_on(McpManager::load_and_probe(cfg, workdir))?,
        Err(_) => {
            // No ambient runtime: build the manager on a temporary one.
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(McpManager::load_and_probe(cfg, workdir))?
        }
    };

    for tool in manager.enabled_tools() {
        registry.register(tool);
    }
    for (name, ok) in manager.status() {
        if !ok {
            warn!(server = %name, "MCP server not active");
        }
    }
    Ok(Some(manager))
}
```

Add to `manager.rs` a constructor that loads config + cache and probes:

```rust
impl McpManager {
    /// Build a manager from config, probing servers whose schema cache is
    /// missing or stale. Never fails on a single server.
    pub async fn load_and_probe(cfg: McpConfig, workdir: &Path) -> Result<Arc<Self>> {
        let mut cache = McpCache::load(workdir);
        let mut servers = HashMap::new();
        let mut cache_dirty = false;

        for (name, sc) in &cfg.mcp_servers {
            let tools = match cache.get_for(name, sc) {
                Some(t) => t.to_vec(),
                None => match probe_tools(sc).await {
                    Ok(probed) => {
                        if let Err(e) = cache.save(workdir) {
                            warn!(error = %e, "failed to persist MCP cache (pre)");
                        }
                        cache.put(name, sc.clone(), probed.clone());
                        cache_dirty = true;
                        probed
                    }
                    Err(e) => {
                        warn!(server = %name, error = %e, "MCP probe failed; server unavailable");
                        Vec::new()
                    }
                },
            };
            servers.insert(name.clone(), ServerEntry {
                config: sc.clone(),
                enabled: AtomicBool::new(sc.enabled),
                tools,
                client: Mutex::new(None),
            });
        }

        if cache_dirty {
            if let Err(e) = cache.save(workdir) {
                warn!(error = %e, "failed to persist MCP cache");
            }
        }

        Ok(Arc::new(Self {
            servers,
            master_enabled: AtomicBool::new(cfg.enabled),
        }))
    }
}

/// Connect once and list tools, then convert to cache entries.
async fn probe_tools(cfg: &ServerConfig) -> Result<Vec<CachedTool>> {
    let client = connect(cfg).await?;
    let tools = client.list_all_tools().await?;
    let mut out = Vec::new();
    for t in tools {
        let read_only = t
            .annotations
            .as_ref()
            .and_then(|a| a.read_only_hint)
            .unwrap_or(false);
        out.push(CachedTool {
            name: t.name.to_string(),
            description: t.description.as_ref().map(|d| d.to_string()),
            input_schema: t.schema_as_json_value(),
            read_only,
        });
    }
    let _ = client.cancel().await;
    Ok(out)
}
```

Add imports to `manager.rs`: `use std::path::Path;`, `use crate::cache::McpCache;`,
`use crate::config::McpConfig;`.

Also add `timeout` around `connect`/`list_all_tools` in `probe_tools`
(e.g. wrap in `tokio::time::timeout(Duration::from_secs(20), ...)`) so a hung
server cannot stall startup.

**Step 4: Run tests to verify they pass**

Run: `cd yi-agent-rs && cargo test -p yi-agent-mcp --test registration`
Expected: PASS (the bad-command case returns quickly with a warning).

**Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-mcp/
git commit -m "feat(mcp): register_mcp_tools with cache probe"
```

---

## Task 9: Wire into headless/app-server bootstrap

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/Cargo.toml` (add `yi-agent-mcp` dep)
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/bootstrap.rs:198` (after process tools)
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/bootstrap.rs` `ToolSetup` struct (add `mcp: Option<Arc<McpManager>>`)

**Step 1: Add the dependency**

In `crates/yi-agent-runtime/Cargo.toml`, add under `[dependencies]`:

```toml
yi-agent-mcp = { workspace = true }
```

**Step 2: Extend `ToolSetup`**

Find the `ToolSetup` struct definition and add a field:

```rust
pub struct ToolSetup {
    pub tools: Arc<yi_agent_core::ToolRegistry>,
    pub catalog: Option<...>,          // existing
    pub system_prompt: Option<...>,    // existing
    pub mcp: Option<Arc<yi_agent_mcp::McpManager>>,
}
```

Update every construction site of `ToolSetup` (the `naked` early-return at
bootstrap.rs:169 and the main return at :200) to set `mcp`.

**Step 3: Register MCP tools in `build_tool_setup_with_switch`**

After `register_process_tools` (bootstrap.rs:198), insert:

```rust
    let mcp = yi_agent_mcp::register_mcp_tools(&mut registry, workspace)?;
```

And return `mcp` in the `ToolSetup`.

For the `naked` early return, use `mcp: None`.

**Step 4: Verify it builds and existing tests pass**

Run: `cd yi-agent-rs && cargo build -p yi-agent-runtime && cargo test -p yi-agent-runtime`
Expected: builds; existing tests pass.

**Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-runtime/
git commit -m "feat(runtime): register MCP tools in shared bootstrap"
```

---

## Task 10: `SlashCommand` and `ControlCommand` variants

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/control_commands.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/slash.rs`

**Step 1: Write the failing test**

Add to `slash.rs` tests:

```rust
    #[test]
    fn mcp_command_is_registered_with_usage() {
        let cmd = SlashCommand::from_name("mcp");
        assert_eq!(cmd, Some(SlashCommand::Mcp));
        assert_eq!(SlashCommand::Mcp.argument_usage(), Some("[on|off|enable <server>|disable <server>|status]"));
        assert!(SlashCommand::all().contains(&SlashCommand::Mcp));
    }
```

**Step 2: Run test to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent --lib tui::slash`
Expected: FAIL.

**Step 3: Implement**

In `control_commands.rs`:
- Add `Mcp` to the `ControlCommand` enum and to `all()`.
- Add a `spec(...)` arm:
  ```rust
  Self::Mcp => spec(self, "mcp", "[on|off|enable <server>|disable <server>|status]", "管理 MCP server 开关", false),
  ```

In `slash.rs`:
- Add `Mcp` to the `SlashCommand` enum.
- Map it in `control_spec()`: `Self::Mcp => ControlCommand::Mcp`.
- Add to `all()`.
- `argument_usage()` and `description()` derive from `control_spec()`
  automatically, so no extra arms are needed — but the `match self` fallbacks
  have exhaustive arms that must include `Self::Mcp`; since `control_spec()`
  returns `Some` for `Mcp`, those fallback arms are unreachable but the compiler
  still requires exhaustiveness. Add `Self::Mcp => "mcp"` / `"管理 MCP server 开关"`
  in `name()` and `description()` (they already early-return via `control_spec`,
  so just add the arms to satisfy exhaustiveness).

Note: `Mcp` is NOT a daemon-backed command; its `ControlCommand` is handled
locally by the driver (Task 11), like `Clear`/`Compact`.

**Step 4: Run test to verify it passes**

Run: `cd yi-agent-rs && cargo test -p yi-agent --lib tui::slash`
Expected: PASS.

**Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/src/
git commit -m "feat(tui): add /mcp slash command"
```

---

## Task 11: Handle `/mcp` in the TUI dispatch and the agent driver

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs` (`execute_slash_command`, ~line 1537)
- Modify: `yi-agent-rs/crates/yi-agent/src/control_commands.rs` (add a payload variant)
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs` (driver loop ~line 1366)

**Step 1: Add a payload-carrying control command**

`ControlCommand` is a fieldless enum today. Add a separate driver-only message
so the local toggle does not pollute the daemon control catalog:

In `control_commands.rs`, add:

```rust
/// Session-local MCP switch actions (handled by the TUI driver, not the daemon).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpToggle {
    Master(bool),
    Server { name: String, on: bool },
}
```

The TUI→driver channel currently carries `ControlCommand`. Introduce a new
channel `mcp_tx: Sender<McpToggle>` alongside `control_tx` in `run_tui_agent`
(main.rs:1318) and pass it into `run_loop` / `execute_slash_command`.

**Step 2: Parser for `/mcp` arguments**

Add to `slash.rs` (or a small helper in `app.rs`):

```rust
/// Parse `/mcp` arguments into a `McpToggle`.
/// Returns an error string for the TUI history on bad input.
pub fn parse_mcp_args(args: &str) -> Result<Option<McpToggle>, String> {
    let parts: Vec<&str> = args.split_whitespace().collect();
    match parts.as_slice() {
        [] | ["status"] => Ok(None),
        ["on"] => Ok(Some(McpToggle::Master(true))),
        ["off"] => Ok(Some(McpToggle::Master(false))),
        ["enable", name] => Ok(Some(McpToggle::Server { name: (*name).into(), on: true })),
        ["disable", name] => Ok(Some(McpToggle::Server { name: (*name).into(), on: false })),
        _ => Err("用法: /mcp [on|off|enable <server>|disable <server>|status]".into()),
    }
}
```

Add unit tests for each arm (on/off/enable/disable/status/invalid).

**Step 3: Dispatch in `execute_slash_command`**

Add an arm (mirroring the `Compact` pattern which sends over `control_tx`):

```rust
SlashCommand::Mcp => {
    match parse_mcp_args(args) {
        Ok(Some(toggle)) => {
            let _ = mcp_tx.try_send(toggle);
            // status will be echoed by the driver after applying
        }
        Ok(None) => {
            let _ = mcp_tx.try_send(/* a Status request */);
        }
        Err(msg) => history.push_line(msg),
    }
}
```

For `status`, add `McpToggle::Status` to the enum and have the driver reply
with a rendered status string via the existing `agent_tx`/history path (see
`Agents` handler for the read-and-print pattern).

**Step 4: Driver handling**

In `run_tui_agent` (main.rs:1314 area):
- Create `let (mcp_tx, mut mcp_rx) = mpsc::channel::<McpToggle>(8);`
- Thread `mcp_tx` into `run_tui_agent`'s call to `run_loop`.
- Pass the `Arc<McpManager>` (from Task 9's `ToolSetup`) into the driver task.
- Extend the `tokio::select!` and `DriverInput` enum to include
  `Mcp(Option<McpToggle>)`.

In the driver loop, on an MCP toggle:

```rust
fn refresh_mcp(registry: &mut ToolRegistry, m: &Arc<McpManager>) {
    for name in m.all_tool_names() {
        registry.remove(&name);
    }
    for tool in m.enabled_tools() {
        registry.register(tool);
    }
}
```

Then rebuild the agent with the updated `current_tools`
(mirroring the `Clear` branch: `agent = Agent::new(..., Arc::clone(&current_tools), ...)`
after mutating a fresh `ToolRegistry` clone). Concretely:

```rust
// on McpToggle::Master(on): m.set_master(on); then:
let mut reg = (*current_tools).clone();
refresh_mcp(&mut reg, &m);
current_tools = Arc::new(reg);
agent = yi_agent_core::Agent::new(
    Arc::clone(&rebuild_provider),
    Arc::clone(&current_tools),
    rebuild_config.clone(),
).with_session(agent.session()).with_permission(
    Arc::clone(&current_checker),
    Arc::clone(&rebuild_decision_rx),
);
```

For `Server { name, on }`: `m.set_server(&name, on)?` then the same refresh.
For `Status`: send a rendered `m.status()` string back to the TUI history.

**Step 5: Verify**

Run: `cd yi-agent-rs && cargo build -p yi-agent && cargo test -p yi-agent --lib`
Expected: builds; slash tests pass.

**Step 6: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/src/
git commit -m "feat(tui): /mcp toggles per-server MCP switches at runtime"
```

---

## Task 12: Teardown wiring

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs` (headless exit ~line 1289; TUI after driver join)
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/bootstrap.rs` (headless setup path)

**Step 1: Ensure connections are closed inside a runtime before process exit**

Headless path uses `std::process::exit(exit_code)` (main.rs:1289), which skips
destructors. Before exiting, call `manager.shutdown().await` inside the async
context (where a tokio runtime is active) so rmcp's child-kill task can run.

Locate `run_headless` and, after the agent stream completes and before
`std::process::exit`, add (inside the async fn / `block_on` scope):

```rust
if let Some(m) = &mcp_manager {
    m.shutdown().await;
}
```

**Step 2: TUI path**

In `run_tui_agent`, after the driver task is joined/aborted and before the
runtime is dropped, call `mcp.shutdown().await` inside the `rt.block_on(...)`
scope. Dropping the manager inside the runtime also triggers rmcp's kill-on-drop.

**Step 3: Verify manually**

Run a session against a cached (no-network) config to confirm no orphan
processes remain after exit:

```bash
cd yi-agent-rs
# (with a valid mcp.json pointing at a real server, if available)
ps aux | grep -i mcp | grep -v grep   # before/after: no leftover server child
```

Expected: no leftover child processes after exit.

**Step 4: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/src/ yi-agent-rs/crates/yi-agent-runtime/src/
git commit -m "fix(mcp): close server connections before process exit"
```

---

## Task 13: In-process integration test (probe → list → call → lazy → toggle)

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-mcp/tests/end_to_end.rs`

This test runs a real rmcp server in-process over a duplex transport and drives
the client through the `Connector` seam. Because `connect()` in `manager.rs`
hard-codes stdio, add a test-only injection point:

**Step 1: Add a test-only connector override**

In `manager.rs`, add:

```rust
/// Test seam: when set, `connect` uses this instead of stdio.
#[cfg(test)]
pub(crate) static TEST_CONNECTOR: std::sync::Mutex<
    Option<Box<dyn Fn() -> tokio::io::DuplexStream + Send + Sync>>
> = std::sync::Mutex::new(None);
```

Refactor `connect` so that when `#[cfg(test)]` and the override is set, it
serves over the injected duplex stream instead of `TokioChildProcess`. (For
integration tests in `tests/`, use a `#[cfg(feature = "test-server")]` gate or
move this scenario into `src/manager.rs` unit tests, which can see private
items. **Recommended: put this end-to-end test inside `src/manager.rs`'s
`mod tests` so it can use the private connector seam.**)

**Step 2: Write the in-process server + test (in `src/manager.rs` `mod tests`)**

```rust
    use rmcp::handler::server::ServerHandler;
    use rmcp::model::{ListToolsResult, Tool};
    use rmcp::service::{RequestContext, RoleServer};

    #[derive(Clone)]
    struct EchoServer;

    impl ServerHandler for EchoServer {
        async fn list_tools(
            &self, _req: Option<rmcp::model::PaginatedRequestParams>,
            _ctx: RequestContext<RoleServer>,
        ) -> Result<ListToolsResult, rmcp::ErrorData> {
            Ok(ListToolsResult {
                tools: vec![Tool::new("echo", "Echoes", serde_json::Map::new())],
                next_cursor: None, meta: None,
            })
        }
        async fn call_tool(
            &self, req: rmcp::model::CallToolRequestParams,
            _ctx: RequestContext<RoleServer>,
        ) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
            Ok(rmcp::model::CallToolResult::success(vec![
                rmcp::model::ContentBlock::text(format!("echo:{:?}", req.arguments)),
            ]))
        }
    }

    #[tokio::test]
    async fn probe_then_call_over_duplex() {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let server = tokio::spawn(async move {
            let running = EchoServer.serve(server_io).await.unwrap();
            let _ = running.waiting().await;
        });
        // ... point the client at a DuplexStream over `client_io`, run
        // probe_tools + call_tool, assert the echo result.
        server.abort();
    }
```

The exact `ServerHandler` signatures and `ListToolsResult`/`CallToolResult`
constructors must be verified against `docs.rs/rmcp/3.5.0`. If the handler
signatures differ, adapt; the goal is a real protocol round-trip.

**Step 3: Assertions to cover**

- `probe_tools` returns one tool named `echo`.
- A cache entry is written after probing (second `McpCache::load` is a hit).
- `call_tool` returns text `echo:{...}` with `is_error == false`.
- Lazy: before the first `call_tool`, the server has received only `initialize`
  + `tools/list` (probe), no `tools/call`.
- Toggle: `set_server("s", false)` then `enabled_tools()` is empty.

**Step 4: Run**

Run: `cd yi-agent-rs && cargo test -p yi-agent-mcp --lib manager::tests`
Expected: PASS.

**Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-mcp/
git commit -m "test(mcp): in-process protocol round-trip"
```

---

## Task 14: Update project-management docs

**Files:**
- Modify: `docs/project-management/yi-agent-mcp.md`
- Modify: `docs/project-management/README.md` (module index count)
- Modify: `docs/project-management/desktop.md` (leave P3 GUI as `[ ]`, but note the backend landed)

**Step 1: Flip the feature checklist**

Change `yi-agent-mcp.md` `[ ] MCP client 与远端 Tool 适配` to `[x]`, and append
verifiable completion criteria (file paths + a runnable command), for example:

```
- [x] MCP client 与远端 Tool 适配
  - 配置解析: `yi-agent-rs/crates/yi-agent-mcp/src/config.rs`
  - 懒连接与调用: `yi-agent-rs/crates/yi-agent-mcp/src/manager.rs`
  - 工具适配: `yi-agent-rs/crates/yi-agent-mcp/src/tool.rs`
  - 注册入口: `yi-agent-rs/crates/yi-agent-mcp/src/lib.rs` `register_mcp_tools`
  - 验证: `cargo test -p yi-agent-mcp`
```

**Step 2: Update the index count**

In `README.md` module index, change `yi-agent-mcp | 0 / 1` to `1 / 1`.

**Step 3: Commit**

```bash
git add docs/project-management/
git commit -m "docs(mcp): mark MCP client implemented"
```

---

## Final verification

Run the focused suites (do NOT run `cargo test --workspace`; see CLAUDE.md):

```bash
cd yi-agent-rs
cargo fmt --all
cargo test -p yi-agent-mcp
cargo test -p yi-agent-core --lib tool
cargo test -p yi-agent --lib tui::slash
cargo build -p yi-agent
```

All must pass before merging. Then follow
`superpowers:finishing-a-development-branch` to merge `feat/mcp-client` into
`main` with `git merge --no-ff`.

## Out of scope (YAGNI)

- HTTP/SSE transport (a `Connector` seam is left for it)
- MCP server side; resources / prompts / sampling / elicitation
- Server health checks / auto-restart (one reconnect attempt per call is the ceiling)
- GUI integration (desktop roadmap P3)
