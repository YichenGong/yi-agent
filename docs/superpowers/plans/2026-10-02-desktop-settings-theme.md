# 桌面端设置界面与主题（通用 Tab）实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 给桌面端加一个设置界面（左下角齿轮入口 + 覆盖层模态 + 「通用」Tab），支持深色/浅色主题，并让 agent 能通过对话切换主题。

**Architecture:** 三块：(1) 前端建一层语义色 token，切 `<html data-theme>` 即整体换肤；(2) app-server 侧新增 `ui/settings/read|write` RPC、`set_theme` 工具与 `ui/settings/updated` 通知，把主题存进共享的 `preferences.json`，工具写后经通知推送回前端；(3) 设置模态只做「通用」Tab，Tab 栏按可扩展设计。

**Tech Stack:** React 19 + TypeScript + Tailwind CSS v4 + Vite + vitest（frontend）；Rust + tokio + serde_json（app-server / core）。

## Global Constraints

- 规格：`docs/superpowers/specs/2026-10-02-desktop-settings-theme-design.md`。
- 主题取值只有 `"dark"` / `"light"` 两个；缺文件 / 损坏 / 未知值一律回退 `"dark"`。
- 持久化位置：`<app-server workdir>/.yi-agent/preferences.json` 的 `theme` 键；**写必须是读-改-写并保留无关键**（该文件与 `subagent_runtime`、`superpowers_kanban` 共用）。
- `preferences.json` 是权威，localStorage（键 `app.theme`）只是首屏缓存。
- 通知方法名 `ui/settings/updated`，参数 `{ theme }`；RPC 方法名 `ui/settings/read`、`ui/settings/write`。
- 工具名 `set_theme`，参数 `{ theme: "dark" | "light" }`。
- 不 token 化状态色（`amber` / `red` / `blue` / `emerald`）与 brand 色。
- 不动 TUI；不给 `yi-agent-tools` / `yi-agent-runtime` 塞前后端融合逻辑。
- 不要在任何 commit message 里写 `Co-Authored-By`。
- 所有改动在 worktree `.worktrees/settings-theme`（分支 `feat/desktop-settings-theme`）内进行，禁止在 `main` 上提交。

## 文件结构

**新增**

- `yi-agent-rs/crates/yi-agent-app-server/src/settings_store.rs` — 主题偏好的读/写（读-改-写 + 原子落盘），无后端依赖，纯文件逻辑。
- `yi-agent-rs/crates/yi-agent-app-server/src/theme_tool.rs` — `ThemeHandle`（当前主题 + 广播）、`SetThemeTool`。
- `desktop/src/lib/theme.ts` — 主题类型、应用/读取、localStorage 缓存。
- `desktop/src/components/SettingsDialog.tsx` — 设置模态（遮罩 + Tab 栏 + 内容）。
- `desktop/src/components/SettingsGeneralTab.tsx` — 「通用」Tab（主题分段控件）。
- `desktop/src/lib/highlight-light.css` — 浅色代码高亮。

**修改**

- `yi-agent-rs/crates/yi-agent-core/src/tool.rs` — 给 `ToolRegistry` 加 `Clone`。
- `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs` — 加 `Notification::UiSettingsUpdated`。
- `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs` — 导出两个新模块。
- `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` — `RuntimeAttachments.theme`、`attach_delegation`/`build_runtime_tooling` 传句柄并注册工具、RPC 分支、通知 watcher、`ui/settings/read|write`。
- `desktop/src/index.css` — 语义 token + `@theme inline`。
- `desktop/index.html` — 内联防闪烁脚本。
- `desktop/src/components/MarkdownText.tsx` — 引浅色高亮并移除深色硬绑定。
- `desktop/src/components/*.tsx`、`desktop/src/App.tsx` — `neutral-*` → 语义类（见 Task 6 映射表）。
- `desktop/src/components/ThreadSidebar.tsx` — 底部 footer + 齿轮按钮。
- `desktop/src/App.tsx` — `settingsOpen` 状态、读写 RPC、通知分支。
- `docs/project-management/desktop.md`、`docs/project-management/README.md` — 进度登记。

---

## Task 1: 主题偏好的读写（settings_store）

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/settings_store.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs`

**Interfaces:**
- Produces:
  - `pub enum Theme { Dark, Light }`，默认 `Dark`
  - `impl Theme { pub fn as_str(&self) -> &'static str; pub fn parse(s: &str) -> Theme }`
  - `pub fn preferences_path(workdir: &Path) -> PathBuf`
  - `pub fn load(workdir: &Path) -> Theme`
  - `pub fn save(workdir: &Path, theme: Theme) -> std::io::Result<()>`

- [ ] **Step 1: 写失败测试**

在 `settings_store.rs` 末尾加：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_defaults_to_dark() {
        let dir = tempfile::TempDir::new().unwrap();
        assert_eq!(load(dir.path()), Theme::Dark);
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::TempDir::new().unwrap();
        save(dir.path(), Theme::Light).unwrap();
        assert_eq!(load(dir.path()), Theme::Light);
    }

    #[test]
    fn malformed_and_unknown_values_fall_back_to_dark() {
        for body in ["not json", "{\"theme\":\"bogus\"}", "[]", "{\"theme\":5}"] {
            let dir = tempfile::TempDir::new().unwrap();
            std::fs::create_dir_all(dir.path().join(".yi-agent")).unwrap();
            std::fs::write(preferences_path(dir.path()), body).unwrap();
            assert_eq!(load(dir.path()), Theme::Dark, "body: {body}");
        }
    }

    #[test]
    fn saving_theme_preserves_unrelated_keys() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".yi-agent")).unwrap();
        std::fs::write(
            preferences_path(dir.path()),
            r#"{"subagent_runtime":"never","superpowers_kanban":true}"#,
        )
        .unwrap();
        save(dir.path(), Theme::Light).unwrap();
        let text = std::fs::read_to_string(preferences_path(dir.path())).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["theme"], "light");
        assert_eq!(value["subagent_runtime"], "never");
        assert_eq!(value["superpowers_kanban"], true);
    }

    #[test]
    fn parsing_is_case_insensitive() {
        assert_eq!(Theme::parse("LIGHT"), Theme::Light);
        assert_eq!(Theme::parse("dark"), Theme::Dark);
        assert_eq!(Theme::parse("  light "), Theme::Light);
        assert_eq!(Theme::parse(""), Theme::Dark);
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server settings_store`
Expected: 编译失败（`settings_store` 模块 / `Theme` 未定义）。

- [ ] **Step 3: 写实现**

```rust
//! 主题偏好：读写 `<workdir>/.yi-agent/preferences.json` 的 `theme` 键。
//!
//! 该文件是共享的（`subagent_runtime`、`superpowers_kanban` 也写它），因此
//! 写必须是读-改-写并保留无关键；落盘用「临时文件 + rename」保证原子性，
//! 与 `yi-agent/src/tui/runtime_prefs.rs` 同一约定。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 桌面端主题。默认深色（与改动前的现状一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    Dark,
    Light,
}

impl Theme {
    pub fn as_str(self) -> &'static str {
        match self {
            Theme::Dark => "dark",
            Theme::Light => "light",
        }
    }

    /// 大小写与空白不敏感；未知值一律回退 `Dark`。
    pub fn parse(s: &str) -> Theme {
        match s.trim().to_ascii_lowercase().as_str() {
            "light" => Theme::Light,
            _ => Theme::Dark,
        }
    }
}

/// `<workdir>/.yi-agent/preferences.json`。
pub fn preferences_path(workdir: &Path) -> PathBuf {
    workdir.join(".yi-agent").join("preferences.json")
}

/// 读主题。缺文件 / 不可读 / 损坏一律回退 `Dark`——坏偏好绝不阻断启动。
pub fn load(workdir: &Path) -> Theme {
    let path = preferences_path(workdir);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(_) => return Theme::Dark,
    };
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(value) => value
            .get("theme")
            .and_then(|v| v.as_str())
            .map(Theme::parse)
            .unwrap_or(Theme::Dark),
        Err(_) => Theme::Dark,
    }
}

/// 写主题：读-改-写，保留无关的顶层键，最后原子替换。
pub fn save(workdir: &Path, theme: Theme) -> std::io::Result<()> {
    let dir = workdir.join(".yi-agent");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("preferences.json");
    let mut object = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default(),
        Err(_) => serde_json::Map::new(),
    };
    object.insert(
        "theme".to_string(),
        serde_json::Value::String(theme.as_str().to_string()),
    );
    let text = serde_json::to_string_pretty(&serde_json::Value::Object(object))
        .map_err(std::io::Error::other)?;
    let tmp_path = dir.join("preferences.json.tmp");
    std::fs::write(&tmp_path, &text)?;
    std::fs::rename(&tmp_path, &path)
}
```

`lib.rs` 加一行：`pub mod settings_store;`（放在 `pub mod server;` 前后按字母序均可）。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server settings_store`
Expected: 5 passed。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/settings_store.rs yi-agent-rs/crates/yi-agent-app-server/src/lib.rs
git commit -m "feat(app-server): persist the desktop theme in preferences.json

Read-modify-write keeps the keys the TUI and the kanban plugin store in
the same file; a missing or corrupt file falls back to dark."
```

---

## Task 2: ThemeHandle 与 SetThemeTool

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/theme_tool.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs`

**Interfaces:**
- Consumes: `settings_store::{Theme, load, save}`。
- Produces:
  - `pub struct ThemeHandle`（`Clone`），方法：
    - `pub fn new(workdir: PathBuf) -> Self`
    - `pub fn current(&self) -> Theme`
    - `pub fn set(&self, theme: Theme)` — 更新内存值、落盘（失败只记日志）、向广播发通知
    - `pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Theme>`
  - `pub struct SetThemeTool { theme: ThemeHandle }`，`impl SetThemeTool { pub fn new(theme: ThemeHandle) -> Self }`，实现 `Tool`。

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use yi_agent_core::Tool;

    #[tokio::test]
    async fn set_theme_writes_preferences_and_broadcasts() {
        let dir = tempfile::TempDir::new().unwrap();
        let handle = ThemeHandle::new(dir.path().to_path_buf());
        assert_eq!(handle.current(), Theme::Dark);
        let mut rx = handle.subscribe();

        let tool = SetThemeTool::new(handle.clone());
        let result = tool.call(serde_json::json!({"theme": "light"})).await;
        assert!(!result.is_error, "tool must succeed");

        // 落盘
        assert_eq!(crate::settings_store::load(dir.path()), Theme::Light);
        // 内存
        assert_eq!(handle.current(), Theme::Light);
        // 广播
        assert_eq!(rx.recv().await.unwrap(), Theme::Light);
    }

    #[tokio::test]
    async fn unknown_theme_is_an_error_and_writes_nothing() {
        let dir = tempfile::TempDir::new().unwrap();
        let handle = ThemeHandle::new(dir.path().to_path_buf());
        let tool = SetThemeTool::new(handle.clone());
        let result = tool.call(serde_json::json!({"theme": "sepia"})).await;
        assert!(result.is_error, "unsupported theme must fail loudly");
        assert_eq!(handle.current(), Theme::Dark);
        assert!(!crate::settings_store::preferences_path(dir.path()).exists());
    }

    #[test]
    fn tool_advertises_its_name_and_schema() {
        let dir = tempfile::TempDir::new().unwrap();
        let tool = SetThemeTool::new(ThemeHandle::new(dir.path().to_path_buf()));
        assert_eq!(tool.name(), "set_theme");
        let schema = tool.schema();
        assert_eq!(schema["required"][0], "theme");
        assert_eq!(schema["properties"]["theme"]["enum"][0], "dark");
        assert_eq!(schema["properties"]["theme"]["enum"][1], "light");
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server theme_tool`
Expected: 编译失败（模块未定义）。

- [ ] **Step 3: 写实现**

```rust
//! 主题句柄与「对话切主题」工具。
//!
//! 前端才是主题的最终渲染方，agent 够不到它；工具只写共享偏好并广播，
//! 由 app-server 的 `ui/settings/updated` 通知把新值推回桌面端。

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::broadcast;
use yi_agent_core::{Tool, ToolMetadata, ToolResult, ToolSource};

use crate::settings_store::{self, Theme};

/// 当前主题 + 变更广播。`Clone` 只复制两个 `Arc`。
#[derive(Clone)]
pub struct ThemeHandle {
    theme: Arc<Mutex<Theme>>,
    tx: broadcast::Sender<Theme>,
    workdir: Arc<PathBuf>,
}

impl ThemeHandle {
    pub fn new(workdir: PathBuf) -> Self {
        let theme = settings_store::load(&workdir);
        let (tx, _rx) = broadcast::channel(16);
        Self {
            theme: Arc::new(Mutex::new(theme)),
            tx,
            workdir: Arc::new(workdir),
        }
    }

    pub fn current(&self) -> Theme {
        *self.theme.lock()
    }

    /// 更新内存值、落盘、广播。落盘失败只记日志：UI 已切了主题，不该因为
    /// 磁盘问题回退，下一次 `ui/settings/write` 会再试。
    pub fn set(&self, theme: Theme) {
        *self.theme.lock() = theme;
        if let Err(error) = settings_store::save(&self.workdir, theme) {
            tracing::warn!(%error, "failed to persist the theme preference");
        }
        let _ = self.tx.send(theme);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Theme> {
        self.tx.subscribe()
    }
}

pub struct SetThemeTool {
    theme: ThemeHandle,
}

#[derive(Debug, Deserialize)]
struct SetThemeArgs {
    theme: String,
}

impl SetThemeTool {
    pub fn new(theme: ThemeHandle) -> Self {
        Self { theme }
    }
}

#[async_trait]
impl Tool for SetThemeTool {
    fn name(&self) -> &str {
        "set_theme"
    }

    fn description(&self) -> &str {
        "Switch the desktop app's color theme. Use when the user asks to change between dark and light mode (e.g. \"switch to light mode\"). This changes only the app's appearance, not any project files."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "theme": {
                    "type": "string",
                    "enum": ["dark", "light"],
                    "description": "The theme to apply."
                }
            },
            "required": ["theme"]
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        let parsed: SetThemeArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolResult::error(format!("invalid arguments: {e}")),
        };
        let normalized = parsed.theme.trim().to_ascii_lowercase();
        let theme = match normalized.as_str() {
            "dark" => Theme::Dark,
            "light" => Theme::Light,
            other => {
                return ToolResult::error(format!(
                    "unsupported theme '{other}': expected 'dark' or 'light'"
                ));
            }
        };
        self.theme.set(theme);
        ToolResult::text(format!("theme set to {}", theme.as_str()))
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            source: ToolSource::Plugin {
                name: "ui".to_string(),
            },
            requires_confirmation: false,
            read_only: false,
            version: None,
        }
    }
}
```

注意：`async_trait` 与 `parking_lot` 需要是 app-server 的**普通依赖**。`async-trait` 现在只在 `[dev-dependencies]`，要在 `yi-agent-rs/crates/yi-agent-app-server/Cargo.toml` 的 `[dependencies]` 加 `async-trait = "0.1"`；`parking_lot` 若 workspace 未提供，则改用 `std::sync::Mutex`（`.lock().unwrap_or_else(|p| p.into_inner())`，与 `server.rs` 既有约定一致）。实现时以 `cargo add` 后的实际类型为准。

`lib.rs` 加：`pub mod theme_tool;`

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server theme_tool`
Expected: 3 passed。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/theme_tool.rs yi-agent-rs/crates/yi-agent-app-server/src/lib.rs yi-agent-rs/crates/yi-agent-app-server/Cargo.toml
git commit -m "feat(app-server): add the set_theme tool and its theme handle

The tool writes the shared preference and broadcasts the change; the
server turns that broadcast into a ui/settings/updated notification."
```

---

## Task 3: 协议与 RPC（ui/settings/read|write、ui/settings/updated）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`

**Interfaces:**
- Consumes: `settings_store::{Theme, load, save}`、`theme_tool::ThemeHandle`。
- Produces:
  - `Notification::UiSettingsUpdated { theme: String }`（wire：`ui/settings/updated`，params `{ theme }`）
  - RPC `ui/settings/read` → `{ "theme": "dark" | "light" }`
  - RPC `ui/settings/write`（params `{ theme }`）→ `{ "ok": true }`

- [ ] **Step 1: 写失败测试**

在 `protocol.rs` 的测试模块加：

```rust
#[test]
fn ui_settings_notification_uses_its_wire_shape() {
    let n = Notification::UiSettingsUpdated {
        theme: "light".into(),
    };
    let v: Value = serde_json::to_value(NotificationEnvelope::new(&n)).unwrap();
    assert_eq!(v["method"], "ui/settings/updated");
    assert_eq!(v["params"]["theme"], "light");
}
```

在 `server.rs` 测试模块加：

```rust
#[tokio::test(flavor = "multi_thread")]
async fn ui_settings_read_and_write_round_trip() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    initialize(&mut h).await;

    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"ui/settings/read","params":{}}"#)
        .await;
    let v = read_response(&mut h, 2).await;
    assert_eq!(v["result"]["theme"], "dark", "default before any write");

    h.send(
        r#"{"jsonrpc":"2.0","id":3,"method":"ui/settings/write","params":{"theme":"light"}}"#,
    )
    .await;
    let v = read_response(&mut h, 3).await;
    assert_eq!(v["result"]["ok"], true);

    h.send(r#"{"jsonrpc":"2.0","id":4,"method":"ui/settings/read","params":{}}"#)
        .await;
    let v = read_response(&mut h, 4).await;
    assert_eq!(v["result"]["theme"], "light");

    // 落盘可被另一个进程读回
    assert_eq!(
        crate::settings_store::load(dir.path()),
        crate::settings_store::Theme::Light
    );
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ui_settings_write_rejects_an_unknown_theme() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    initialize(&mut h).await;

    h.send(
        r#"{"jsonrpc":"2.0","id":2,"method":"ui/settings/write","params":{"theme":"sepia"}}"#,
    )
    .await;
    let v = read_response(&mut h, 2).await;
    assert!(v.get("error").is_some(), "unsupported theme must be rejected");
    h.shutdown().await;
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server ui_settings`
Expected: 编译失败（`UiSettingsUpdated` 未定义 / 方法未实现 → `method not found`）。

- [ ] **Step 3: 写实现**

`protocol.rs` 的 `Notification` 枚举里，`ProcessUpdated` 之后加：

```rust
    /// 桌面端 UI 偏好（当前只有主题）发生变化。
    ///
    /// 只由 `ThemeHandle` 的广播触发；客户端收到即把 `data-theme` 换成新值。
    #[serde(rename = "ui/settings/updated")]
    UiSettingsUpdated { theme: String },
```

`server.rs` 修改：

1) `run_with` 的 `RuntimeAttachments` 解构处，拿到 theme 句柄。先在 `struct RuntimeAttachments` 加字段：

```rust
struct RuntimeAttachments {
    runtimes: ProjectRuntimes,
    thread_roots: ThreadRoots,
    theme: crate::theme_tool::ThemeHandle,
}
```

2) `run()` 里构造并传入：

```rust
pub async fn run<R, W>(reader: R, writer: W, cfg: RuntimeConfig) -> anyhow::Result<()> {
    let cfg_for_factory = cfg.clone();
    let workspaces = Arc::new(WorkspaceIndex::new(crate::workspace_index::default_path()));
    let runtimes: ProjectRuntimes = Arc::new(StdMutex::new(HashMap::new()));
    let theme = crate::theme_tool::ThemeHandle::new(cfg.workdir.clone());
    // ... run_with(..., RuntimeAttachments { runtimes, thread_roots, theme }, factory)
}
```

3) `run_with` 里，解构出 theme 并起一个通知 watcher（放在 `process_watches` 声明附近；watcher 与 thread 无关，全局一个）：

```rust
    let RuntimeAttachments { runtimes, thread_roots, theme } = attachments;

    // 主题变化 → ui/settings/updated（全局一条流，与 thread 无关）。
    {
        let writer = Arc::clone(&writer);
        let mut rx = theme.subscribe();
        tokio::spawn(async move {
            while let Ok(theme) = rx.recv().await {
                let n = Notification::UiSettingsUpdated {
                    theme: theme.as_str().to_string(),
                };
                if write_notification(&writer, &n).await.is_err() {
                    return;
                }
            }
        });
    }
```

4) 方法分派里，`"config/read"` 分支之后加：

```rust
                    "ui/settings/read" => {
                        let theme = crate::settings_store::load(&cfg.workdir);
                        write_response(
                            &writer,
                            ok_response(id, json!({ "theme": theme.as_str() })),
                        )
                        .await?;
                    }
                    "ui/settings/write" => {
                        let requested = req
                            .params
                            .get("theme")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let theme = match requested.trim().to_ascii_lowercase().as_str() {
                            "dark" => crate::settings_store::Theme::Dark,
                            "light" => crate::settings_store::Theme::Light,
                            other => {
                                write_response(
                                    &writer,
                                    err_response(
                                        id,
                                        RpcError::invalid_params(format!(
                                            "unsupported theme '{other}': expected 'dark' or 'light'"
                                        )),
                                    ),
                                )
                                .await?;
                                continue;
                            }
                        };
                        theme_handle.set(theme);
                        write_response(&writer, ok_response(id, json!({ "ok": true }))).await?;
                    }
```

其中 `theme_handle` 是主循环里 `theme.clone()` 出来的本地变量（`theme` 被 watcher 借走后仍需写；在两个地方各 `clone()` 一次即可）。watcher 用 `theme.subscribe()`，RPC 分支用 `theme_handle.clone()` 调 `set`。

**连带改动（共 4 处 `RuntimeAttachments { … }` 字面量必须补 `theme` 字段）**：
`run()` 的构造点（`server.rs:786` 附近）用生产句柄；
`Harness::with_config`（`server.rs:4009`）与另外 3 处测试构造点（`4592` / `4649` / `4710`）用测试辅助。在测试模块加：

```rust
    fn test_theme() -> crate::theme_tool::ThemeHandle {
        crate::theme_tool::ThemeHandle::new(std::env::temp_dir())
    }
```

并给这些字面量补 `theme: test_theme(),`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server ui_settings && cargo test -p yi-agent-app-server protocol`
Expected: 全绿。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): add ui/settings RPCs and the settings notification

ui/settings/read and write back the settings dialog; the theme handle's
broadcast becomes a ui/settings/updated notification so a theme set by
any path reaches the client."
```

---

## Task 4: 把 set_theme 工具注册进每个 thread

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/tool.rs`（`ToolRegistry` 加 `Clone`）
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs`（加 `permission_checker()` / `decision_rx()` 取回器）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`

**Interfaces:**
- Consumes: `theme_tool::{ThemeHandle, SetThemeTool}`。
- Produces: 每个 app-server thread 的 agent 工具集都含 `set_theme`（委派可用与不可用两条路径都在）。

- [ ] **Step 1: 写失败测试**

在 `server.rs` 测试模块加（`register_theme_tool` 是同模块私有函数，测试可直接调用）：

```rust
#[test]
fn the_theme_tool_is_registered_into_a_thread_registry() {
    let dir = tempfile::TempDir::new().unwrap();
    let handle = crate::theme_tool::ThemeHandle::new(dir.path().to_path_buf());
    let mut registry = yi_agent_core::ToolRegistry::new();
    register_theme_tool(&mut registry, handle);
    assert!(
        registry.get("set_theme").is_some(),
        "a thread registry must carry the theme tool"
    );
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server the_theme_tool_is_registered`
Expected: 编译失败（`register_theme_tool` 未定义）。

- [ ] **Step 3: 写实现**

`yi-agent-core/src/tool.rs`：给 `ToolRegistry` 加 `Clone`。

```rust
#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}
```

`server.rs`：加一个 server 局部辅助函数（放在 `build_runtime_tooling` 旁）：

```rust
/// 把主题工具注册进一个 thread 的工具集。
///
/// 两条路径都要注册：委派可用时走 `build_runtime_tooling` 的 registry，
/// 不可用时走 factory 里用 bootstrap `tools` 克隆出来的 registry——否则
/// 非 git 目录下自然语言切主题会静默失效。
fn register_theme_tool(
    registry: &mut yi_agent_core::ToolRegistry,
    theme: crate::theme_tool::ThemeHandle,
) {
    registry.register(Arc::new(crate::theme_tool::SetThemeTool::new(theme)));
}
```

在 `build_runtime_tooling` 的签名加 `theme: crate::theme_tool::ThemeHandle` 参数，并在它 `Ok(RuntimeTooling { ... })` 之前调用：

```rust
    register_theme_tool(&mut registry, theme);
```

`attach_delegation` 签名加 `theme: &crate::theme_tool::ThemeHandle`，透传给 `build_runtime_tooling`（`theme.clone()`）；两个调用点（`server.rs:1156`、`1393`）与 3 处 `build_runtime_tooling` 测试调用点（`3428` / `3503` / `3543`）都要补这个参数（测试里传 `&test_theme()`）。

在 factory 闭包（`run()` 内）里，委派不可用时的 registry 也要带工具：

```rust
            let built = yi_agent_runtime::bootstrap::bootstrap_agent(
                &thread_cfg,
                yi_agent_runtime::bootstrap::PermissionMode::Interactive,
            )?;
            // 主题工具必须进每个 thread 的工具集；委派随后可能再用
            // `wrap_for_delegation` 换掉 registry，那条路径同样注册。
            // `Agent::new` 会顺带清掉权限检查器，故重建后必须重新装上
            // （否则非 git 目录下会退化成不弹审批）。
            let (permission, decision_rx) =
                (built.agent.permission_checker(), built.agent.decision_rx());
            let mut registry = (*built.tools).clone();
            register_theme_tool(&mut registry, theme_for_factory.clone());
            let mut agent = yi_agent_core::Agent::new(
                built.provider.clone(),
                Arc::new(registry),
                built.agent.config().clone(),
            )
            .with_session(built.agent.session());
            if let (Some(checker), Some(rx)) = (permission, decision_rx) {
                agent = agent.with_permission(checker, rx);
            }
            Ok(BuiltAgent {
                agent: apply_session(agent, session),
                provider: built.provider,
                config,
                decision_tx: built.decision_tx,
                decision_rx: built.decision_rx,
                catalog: built.catalog,
                yolo: built.yolo,
                process_manager: built.process_manager,
            })
```

**实现注意**：`Agent` 目前**没有**暴露 `permission_checker()` / `decision_rx()` 取回器（`with_permission` 只进不出）。因此先给 `yi-agent-core/src/agent.rs` 的 `impl Agent` 加两个方法：

```rust
    /// The permission checker the agent currently holds, if any. Lets a caller
    /// that must rebuild the agent (new tool set) re-attach the same approval
    /// path instead of silently dropping it.
    pub fn permission_checker(&self) -> Option<Arc<crate::permission::PermissionChecker>> {
        self.permission_checker.clone()
    }

    /// The decision receiver the agent currently holds, if any.
    pub fn decision_rx(&self) -> Option<DecisionRx> {
        self.decision_rx.clone()
    }
```

（`DecisionRx` 是 `agent.rs` 内的类型别名，按该文件当前拼写使用。）若嫌接口外露，可改为在 `BuiltAgent` 里直接存 `permission` / `decision_rx` 以省去重建——两种都行，但**必须**保证重建后的 agent 仍带审批路径。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server && cargo test -p yi-agent-core`
Expected: 全绿（含既有 169 个 app-server 测试）。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-core/src/tool.rs yi-agent-rs/crates/yi-agent-core/src/agent.rs yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): give every thread agent the set_theme tool

Registered on both the delegation and the non-delegation registry paths,
so talking to the agent can switch the theme in any working directory."
```

---

## Task 5: 前端主题模块与 token 层

**Files:**
- Create: `desktop/src/lib/theme.ts`
- Create: `desktop/src/lib/theme.test.ts`
- Modify: `desktop/src/index.css`
- Modify: `desktop/index.html`

**Interfaces:**
- Produces:
  - `export type Theme = "dark" | "light";`
  - `export const THEME_STORAGE_KEY = "app.theme";`
  - `export function applyTheme(theme: Theme): void` — 设 `document.documentElement.dataset.theme` 并写 localStorage
  - `export function readCachedTheme(): Theme | null`
  - `export function parseTheme(value: unknown): Theme` — 非 `"light"` 一律 `"dark"`

- [ ] **Step 1: 写失败测试**

`desktop/src/lib/theme.test.ts`：

```ts
/** @vitest-environment jsdom */
import { afterEach, describe, expect, it } from "vitest";
import { applyTheme, parseTheme, readCachedTheme, THEME_STORAGE_KEY } from "./theme";

afterEach(() => {
  localStorage.clear();
  delete document.documentElement.dataset.theme;
});

describe("parseTheme", () => {
  it("only light is light; everything else is dark", () => {
    expect(parseTheme("light")).toBe("light");
    expect(parseTheme("dark")).toBe("dark");
    expect(parseTheme("sepia")).toBe("dark");
    expect(parseTheme(undefined)).toBe("dark");
    expect(parseTheme(5)).toBe("dark");
  });

  it("trims and lowercases", () => {
    expect(parseTheme(" LIGHT ")).toBe("light");
  });
});

describe("applyTheme", () => {
  it("sets data-theme and caches the value", () => {
    applyTheme("light");
    expect(document.documentElement.dataset.theme).toBe("light");
    expect(localStorage.getItem(THEME_STORAGE_KEY)).toBe("light");
  });
});

describe("readCachedTheme", () => {
  it("returns the cached theme, or null when absent", () => {
    expect(readCachedTheme()).toBeNull();
    localStorage.setItem(THEME_STORAGE_KEY, "light");
    expect(readCachedTheme()).toBe("light");
  });

  it("treats a junk cache as absent", () => {
    localStorage.setItem(THEME_STORAGE_KEY, "sepia");
    expect(readCachedTheme()).toBe("dark");
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/lib/theme.test.ts`
Expected: FAIL（模块不存在）。

- [ ] **Step 3: 写实现**

`desktop/src/lib/theme.ts`：

```ts
/** 桌面端主题。`preferences.json` 是权威，localStorage 只做首屏缓存。 */
export type Theme = "dark" | "light";

export const THEME_STORAGE_KEY = "app.theme";

/** 非 "light" 一律 dark —— 坏值不许把界面留在未定义状态。 */
export function parseTheme(value: unknown): Theme {
  return typeof value === "string" && value.trim().toLowerCase() === "light"
    ? "light"
    : "dark";
}

/** 首屏缓存读取；localStorage 在受限环境会抛，吞掉视为未缓存。 */
export function readCachedTheme(): Theme | null {
  try {
    const raw = localStorage.getItem(THEME_STORAGE_KEY);
    return raw === null ? null : parseTheme(raw);
  } catch {
    return null;
  }
}

/** 应用主题：切 <html> 的 data-theme 并回写首屏缓存。 */
export function applyTheme(theme: Theme): void {
  document.documentElement.dataset.theme = theme;
  try {
    localStorage.setItem(THEME_STORAGE_KEY, theme);
  } catch {
    /* 缓存写失败不影响渲染 */
  }
}
```

`desktop/src/index.css` 整体替换为：

```css
@import "tailwindcss";
@plugin "@tailwindcss/typography";

/* 语义色 token。组件只引用这些语义名，不写字面色阶，切 data-theme 即整体换肤。 */
:root {
  --surface: #0a0a0a;
  --panel: #171717;
  --raised: #262626;
  --line: #262626;
  --line-strong: #404040;
  --fg: #f5f5f5;
  --fg-muted: #a3a3a3;
  --fg-subtle: #737373;
  --fg-faint: #525252;
}

[data-theme="light"] {
  --surface: #ffffff;
  --panel: #f5f5f5;
  --raised: #e5e5e5;
  --line: #e5e5e5;
  --line-strong: #d4d4d4;
  --fg: #171717;
  --fg-muted: #525252;
  --fg-subtle: #737373;
  --fg-faint: #a3a3a3;
}

/* 把 token 暴露成 Tailwind 颜色工具类：bg-surface / text-fg / border-line … */
@theme inline {
  --color-surface: var(--surface);
  --color-panel: var(--panel);
  --color-raised: var(--raised);
  --color-line: var(--line);
  --color-line-strong: var(--line-strong);
  --color-fg: var(--fg);
  --color-fg-muted: var(--fg-muted);
  --color-fg-subtle: var(--fg-subtle);
  --color-fg-faint: var(--fg-faint);
}
```

`desktop/index.html` 的 `<head>` 加内联脚本（必须早于模块脚本，避免首帧闪深色）：

```html
    <script>
      // 首帧前应用缓存主题，避免闪一下深色。缓存缺失/损坏时保持默认深色。
      try {
        var t = localStorage.getItem("app.theme");
        if (typeof t === "string" && t.trim().toLowerCase() === "light") {
          document.documentElement.dataset.theme = "light";
        }
      } catch (e) {}
    </script>
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/lib/theme.test.ts && npx tsc --noEmit`
Expected: 全绿。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/lib/theme.ts desktop/src/lib/theme.test.ts desktop/src/index.css desktop/index.html
git commit -m "feat(desktop): add semantic color tokens and a theme module

Tokens replace literal neutral-* usage so switching data-theme reskins
every component; a head script applies the cached theme before first paint."
```

---

## Task 6: 把硬编码配色迁到语义 token

**Files:**
- Modify: `desktop/src/components/*.tsx`、`desktop/src/App.tsx`（含 `neutral-*` 的 18 个源文件）

**映射表（逐字符替换，`neutral-*` → 语义类）：**

| 原类 | 新类 |
|---|---|
| `bg-neutral-950` | `bg-surface` |
| `bg-neutral-900` | `bg-panel` |
| `bg-neutral-800` | `bg-raised` |
| `bg-neutral-925` | `bg-raised`（原为无效色阶，等于没背景；见 Step 3） |
| `border-neutral-800` | `border-line` |
| `border-neutral-700` | `border-line-strong` |
| `text-neutral-100` / `text-neutral-200` | `text-fg` |
| `text-neutral-300` / `text-neutral-400` | `text-fg-muted` |
| `text-neutral-500` | `text-fg-subtle` |
| `text-neutral-600` | `text-fg-faint` |
| `divide-neutral-800` | `divide-line` |
| `hover:text-neutral-200` / `hover:text-neutral-300` | `hover:text-fg` / `hover:text-fg-muted` |
| `hover:bg-neutral-700` | `hover:bg-raised` |
| `hover:bg-neutral-800/50`、`focus:bg-neutral-800/50` | `hover:bg-raised/50`、`focus:bg-raised/50` |
| `border-t-neutral-300` | `border-t-fg` |
| `hover:bg-neutral-700/50` | `hover:bg-raised/50` |

- [ ] **Step 1: 记录迁移前基线**

Run: `cd desktop && npx vitest run`
Expected: 全绿（作为回归基线）。

- [ ] **Step 2: 执行替换**

对每个文件按映射表替换。范围：

```bash
grep -rl "neutral-" desktop/src --include=*.tsx
```

共 20 个文件（含 2 个测试文件 `ThreadSidebar.test.tsx`、`TitleBar.test.tsx`——它们断言的是类名，一并按映射表更新，使断言与新类名一致）。

- [ ] **Step 3: 修复无效的 `bg-neutral-925`**

`SuperpowersKanbanCollapsedStrip.tsx:18` 与 `SubagentRail.tsx:41` 原有的 `bg-neutral-925` 不是 Tailwind 色阶，**此前不产生任何背景**。按映射表改成 `bg-raised`，这两个侧条才真正有底色。

- [ ] **Step 4: 验证零残留并回归**

Run:
```bash
cd desktop && grep -rn "neutral-" src --include=*.tsx | grep -v "test" ; npx vitest run && npx tsc --noEmit && npm run build
```
Expected: `grep` 无输出（源文件零残留）；测试、类型、构建全绿。

- [ ] **Step 5: 提交**

```bash
git add desktop/src
git commit -m "refactor(desktop): replace literal neutral-* colors with semantic tokens

Lets data-theme reskin every component without touching markup. Also fixes
two panels whose bg-neutral-925 (not a Tailwind shade) rendered no background."
```

---

## Task 7: 浅色下的代码高亮

**Files:**
- Create: `desktop/src/lib/highlight-light.css`
- Modify: `desktop/src/components/MarkdownText.tsx`

- [ ] **Step 1: 写实现（无单测：纯样式，用构建 + 目视验证）**

`desktop/src/lib/highlight-light.css`：在 `[data-theme="light"]` 作用域下覆写 highlight.js 的令牌色，保证浅底深字可读：

```css
/* 浅色主题下的代码高亮。深色由 highlight.js 的 github-dark 主题承担。 */
[data-theme="light"] .hljs {
  color: #24292e;
  background: #f6f8fa;
}
[data-theme="light"] .hljs-comment,
[data-theme="light"] .hljs-quote {
  color: #6a737d;
}
[data-theme="light"] .hljs-keyword,
[data-theme="light"] .hljs-selector-tag,
[data-theme="light"] .hljs-built_in {
  color: #d73a49;
}
[data-theme="light"] .hljs-string,
[data-theme="light"] .hljs-attr {
  color: #032f62;
}
[data-theme="light"] .hljs-number,
[data-theme="light"] .hljs-literal {
  color: #005cc5;
}
[data-theme="light"] .hljs-title,
[data-theme="light"] .hljs-function .hljs-title {
  color: #6f42c1;
}
```

`MarkdownText.tsx` 顶部保持 `import "highlight.js/styles/github-dark.css";`，在其后加：

```ts
import "../lib/highlight-light.css";
```

- [ ] **Step 2: 构建验证**

Run: `cd desktop && npx tsc --noEmit && npm run build`
Expected: 全绿；`dist` 产物里含 `hljs` 浅色覆写规则。

- [ ] **Step 3: 提交**

```bash
git add desktop/src/lib/highlight-light.css desktop/src/components/MarkdownText.tsx
git commit -m "feat(desktop): readable code highlighting in the light theme"
```

---

## Task 8: 设置模态与「通用」Tab

**Files:**
- Create: `desktop/src/components/SettingsDialog.tsx`
- Create: `desktop/src/components/SettingsGeneralTab.tsx`
- Create: `desktop/src/components/SettingsDialog.test.tsx`

**Interfaces:**
- Consumes: `lib/theme::{Theme}`。
- Produces:
  - `SettingsDialog({ open, theme, onThemeChange, onClose })`：`open` 为 false 时不渲染；Esc / 点遮罩 / 关闭按钮调 `onClose`；左侧 Tab 栏含唯一项「通用」。
  - `SettingsGeneralTab({ theme, onThemeChange })`：两个按钮（深色 / 浅色），当前项 `aria-pressed` 为 true。

- [ ] **Step 1: 写失败测试**

```tsx
/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { SettingsDialog } from "./SettingsDialog";

afterEach(cleanup);

describe("SettingsDialog", () => {
  it("renders nothing while closed", () => {
    render(
      <SettingsDialog open={false} theme="dark" onThemeChange={() => {}} onClose={() => {}} />,
    );
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("shows the 通用 tab with both theme choices", () => {
    render(
      <SettingsDialog open theme="dark" onThemeChange={() => {}} onClose={() => {}} />,
    );
    expect(screen.getByRole("dialog")).toBeTruthy();
    expect(screen.getByRole("tab", { name: "通用" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "深色" }).getAttribute("aria-pressed")).toBe("true");
    expect(screen.getByRole("button", { name: "浅色" }).getAttribute("aria-pressed")).toBe("false");
  });

  it("reports the chosen theme", () => {
    const onThemeChange = vi.fn();
    render(
      <SettingsDialog open theme="dark" onThemeChange={onThemeChange} onClose={() => {}} />,
    );
    fireEvent.click(screen.getByRole("button", { name: "浅色" }));
    expect(onThemeChange).toHaveBeenCalledWith("light");
  });

  it("closes on Escape and on the backdrop", () => {
    const onClose = vi.fn();
    const { container } = render(
      <SettingsDialog open theme="dark" onThemeChange={() => {}} onClose={onClose} />,
    );
    fireEvent.keyDown(document, { key: "Escape" });
    expect(onClose).toHaveBeenCalledTimes(1);
    const backdrop = container.querySelector("[data-settings-backdrop]");
    expect(backdrop).toBeTruthy();
    fireEvent.click(backdrop!);
    expect(onClose).toHaveBeenCalledTimes(2);
  });

  it("closes from the explicit close button", () => {
    const onClose = vi.fn();
    render(<SettingsDialog open theme="dark" onThemeChange={() => {}} onClose={onClose} />);
    fireEvent.click(screen.getByRole("button", { name: "关闭设置" }));
    expect(onClose).toHaveBeenCalledTimes(1);
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/components/SettingsDialog.test.tsx`
Expected: FAIL（组件不存在）。

- [ ] **Step 3: 写实现**

`desktop/src/components/SettingsGeneralTab.tsx`：

```tsx
import type { Theme } from "../lib/theme";

const CHOICES: Array<{ value: Theme; label: string }> = [
  { value: "dark", label: "深色" },
  { value: "light", label: "浅色" },
];

export function SettingsGeneralTab({
  theme,
  onThemeChange,
}: {
  theme: Theme;
  onThemeChange: (theme: Theme) => void;
}) {
  return (
    <section className="p-5">
      <h2 className="text-sm font-medium text-fg">主题</h2>
      <div className="mt-3 inline-flex rounded-md border border-line overflow-hidden">
        {CHOICES.map((c) => {
          const active = c.value === theme;
          return (
            <button
              key={c.value}
              type="button"
              aria-pressed={active}
              onClick={() => onThemeChange(c.value)}
              className={`px-4 py-1.5 text-sm ${
                active ? "bg-raised text-fg" : "text-fg-muted hover:bg-raised/50"
              }`}
            >
              {c.label}
            </button>
          );
        })}
      </div>
      <p className="mt-4 text-xs text-fg-subtle">
        也可以直接对话让 Yi-Agent 切换主题，例如「帮我切成浅色」。
      </p>
    </section>
  );
}
```

`desktop/src/components/SettingsDialog.tsx`：

```tsx
import { useEffect } from "react";
import type { Theme } from "../lib/theme";
import { SettingsGeneralTab } from "./SettingsGeneralTab";

/** 左侧 Tab 栏。目前只有「通用」，后续 Tab 只需往这张表里加项。 */
const TABS = [{ id: "general", label: "通用" }] as const;

export function SettingsDialog({
  open,
  theme,
  onThemeChange,
  onClose,
}: {
  open: boolean;
  theme: Theme;
  onThemeChange: (theme: Theme) => void;
  onClose: () => void;
}) {
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [open, onClose]);

  if (!open) return null;

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center">
      <div
        data-settings-backdrop
        className="absolute inset-0 bg-black/50"
        onClick={onClose}
      />
      <div
        role="dialog"
        aria-modal="true"
        aria-label="设置"
        className="relative flex h-[70vh] w-[70vw] max-w-3xl overflow-hidden rounded-lg border border-line bg-panel text-fg shadow-2xl"
      >
        <nav
          role="tablist"
          aria-orientation="vertical"
          className="flex w-40 shrink-0 flex-col border-r border-line bg-surface p-2"
        >
          {TABS.map((t) => (
            <button
              key={t.id}
              role="tab"
              aria-selected="true"
              className="rounded px-3 py-1.5 text-left text-sm text-fg-muted hover:bg-raised/50 aria-selected:text-fg"
            >
              {t.label}
            </button>
          ))}
        </nav>
        <div className="flex min-w-0 flex-1 flex-col">
          <div className="flex items-center justify-between border-b border-line px-4 py-2">
            <span className="text-xs text-fg-subtle">设置</span>
            <button
              type="button"
              aria-label="关闭设置"
              onClick={onClose}
              className="rounded px-2 text-fg-subtle hover:text-fg"
            >
              ×
            </button>
          </div>
          <div className="min-h-0 flex-1 overflow-y-auto">
            <SettingsGeneralTab theme={theme} onThemeChange={onThemeChange} />
          </div>
        </div>
      </div>
    </div>
  );
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/components/SettingsDialog.test.tsx`
Expected: 5 passed。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/components/SettingsDialog.tsx desktop/src/components/SettingsGeneralTab.tsx desktop/src/components/SettingsDialog.test.tsx
git commit -m "feat(desktop): settings dialog with an extensible tab rail

The 通用 tab hosts the theme switch; the rail is a table so later tabs
(Model Provider / Agent / Tools) are additive."
```

---

## Task 9: 侧栏左下角的设置入口

**Files:**
- Modify: `desktop/src/components/ThreadSidebar.tsx`
- Modify: `desktop/src/components/ThreadSidebar.test.tsx`

**Interfaces:**
- Produces: `ThreadSidebar` 新增 prop `onOpenSettings: () => void`；底部 footer 渲染 `aria-label="设置"` 的齿轮按钮，点击调用它。

- [ ] **Step 1: 写失败测试**

`ThreadSidebar.test.tsx` 的 `sidebarProps()` 显式列出全部 props，新增必填 prop 后必须给它补默认值，否则所有既有用例类型报错。先在 `sidebarProps` 里加一行：

```tsx
    onBrowse: vi.fn(),
    onOpenSettings: vi.fn(),
```

再加用例：

```tsx
  it("offers a settings button in the footer", () => {
    const onOpenSettings = vi.fn();
    renderSidebar({ onOpenSettings });
    fireEvent.click(screen.getByRole("button", { name: "设置" }));
    expect(onOpenSettings).toHaveBeenCalledTimes(1);
  });
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/components/ThreadSidebar.test.tsx`
Expected: FAIL（无 `aria-label="设置"` 的按钮）。

- [ ] **Step 3: 写实现**

`ThreadSidebar` 的 props 解构与类型加 `onOpenSettings: () => void`。在滚动区 `</div>` 之后、`role="separator"` 之前插入 footer：

```tsx
      <div className="mt-auto flex items-center border-t border-line p-2">
        <button
          type="button"
          aria-label="设置"
          title="设置"
          onClick={onOpenSettings}
          className="rounded p-1 text-fg-subtle hover:bg-raised/50 hover:text-fg"
        >
          <svg viewBox="0 0 16 16" className="size-4" fill="currentColor" aria-hidden="true">
            <path d="M8 10.5a2.5 2.5 0 1 0 0-5 2.5 2.5 0 0 0 0 5Z" />
            <path d="M6.9 1.3a.7.7 0 0 0-.7.6l-.1.9a5.4 5.4 0 0 0-.9.5l-.9-.3a.7.7 0 0 0-.8.3l-.7 1.2a.7.7 0 0 0 .1.9l.7.6a5.5 5.5 0 0 0 0 1l-.7.6a.7.7 0 0 0-.1.9l.7 1.2a.7.7 0 0 0 .8.3l.9-.3c.3.2.6.4.9.5l.1.9a.7.7 0 0 0 .7.6h1.4a.7.7 0 0 0 .7-.6l.1-.9c.3-.1.6-.3.9-.5l.9.3a.7.7 0 0 0 .8-.3l.7-1.2a.7.7 0 0 0-.1-.9l-.7-.6a5.5 5.5 0 0 0 0-1l.7-.6a.7.7 0 0 0 .1-.9l-.7-1.2a.7.7 0 0 0-.8-.3l-.9.3a5.4 5.4 0 0 0-.9-.5l-.1-.9a.7.7 0 0 0-.7-.6H6.9Zm1.1 8a1.6 1.6 0 1 1 0-3.2 1.6 1.6 0 0 1 0 3.2Z" />
          </svg>
        </button>
      </div>
```

滚动区已有 `flex-1`，footer 用 `mt-auto` 兜底贴底；`aside` 本身是 `flex-col`，无需改布局。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/components/ThreadSidebar.test.tsx`
Expected: 全绿（含既有 29 例）。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/components/ThreadSidebar.tsx desktop/src/components/ThreadSidebar.test.tsx
git commit -m "feat(desktop): a settings button in the sidebar footer"
```

---

## Task 10: App 接线（打开设置、读写主题、通知响应）

**Files:**
- Modify: `desktop/src/App.tsx`
- Modify: `desktop/src/App.test.tsx`

**Interfaces:**
- Consumes: `lib/theme::{Theme, applyTheme, parseTheme}`、`SettingsDialog`、`ThreadSidebar.onOpenSettings`。
- Produces: App 持有 `theme` 状态；首帧后 `ui/settings/read` 拉取权威值；`onThemeChange` 走 `ui/settings/write`；通知分支处理 `ui/settings/updated`。

- [ ] **Step 1: 写失败测试**

在 `App.test.tsx` 加（沿用文件既有 mock transport 与 render 辅助）：

```tsx
import { act } from "@testing-library/react";

/** 推进到 initialize 之后（App 挂载即发 initialize）。 */
async function settle() {
  await waitFor(() => expect(clients[0].requests.some((r) => r.method === "initialize")).toBe(true));
}

/** 把一帧服务端通知交给 App 注册的通知回调。 */
function notify(method: string, params: unknown) {
  act(() => {
    for (const cb of state.notifHandlers) cb({ method, params });
  });
}

it("applies the theme returned by ui/settings/read", async () => {
  render(<App />);
  await settle();
  await waitFor(() => expect(document.documentElement.dataset.theme).toBe("dark"));
});

it("writes the theme when the settings dialog switches it", async () => {
  render(<App />);
  await settle();
  fireEvent.click(screen.getByRole("button", { name: "设置" }));
  fireEvent.click(screen.getByRole("button", { name: "浅色" }));
  await waitFor(() =>
    expect(clients[0].requests).toContainEqual({
      method: "ui/settings/write",
      params: { theme: "light" },
    }),
  );
  expect(document.documentElement.dataset.theme).toBe("light");
});

it("follows a ui/settings/updated notification", async () => {
  render(<App />);
  await settle();
  await waitFor(() => expect(document.documentElement.dataset.theme).toBe("dark"));
  notify("ui/settings/updated", { theme: "light" });
  await waitFor(() => expect(document.documentElement.dataset.theme).toBe("light"));
});
```

为了让装配稳定，在 `App.test.tsx` 顶部加（并 `import { render } from "@testing-library/react"` 已有）：

```tsx
import { beforeEach } from "vitest";
beforeEach(() => {
  // 主题是全局 DOM 状态，用例间必须清掉，否则首例会污染后续。
  delete document.documentElement.dataset.theme;
  localStorage.clear();
});
```

并在 `vi.mock("./lib/rpc", ...)` 的 `request` 里，`thread/start` 分支之后补：

```ts
      if (method === "ui/settings/read") return { theme: "dark" };
      if (method === "ui/settings/write") return { ok: true };
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/App.test.tsx`
Expected: 新增三例 FAIL（`ui/settings/read` 未被请求、无「设置」按钮触发、主题不跟随）。

- [ ] **Step 3: 写实现**

`App.tsx`：

1. 导入：

```tsx
import { applyTheme, parseTheme, readCachedTheme, type Theme } from "./lib/theme";
import { SettingsDialog } from "./components/SettingsDialog";
```

2. 状态：

```tsx
  const [settingsOpen, setSettingsOpen] = useState(false);
  // 首屏先用缓存渲染，连上后以 ui/settings/read 的权威值为准。
  const [theme, setTheme] = useState<Theme>(() => readCachedTheme() ?? "dark");
```

3. 应用主题（`useEffect`）：

```tsx
  useEffect(() => {
    applyTheme(theme);
  }, [theme]);
```

4. 通知分支（在 `client.onNotification` 内，`agent/trace/event` 分支之前加）：

```tsx
      if (n.method === "ui/settings/updated") {
        // 对话（set_theme 工具）改了主题：跟随它。
        setTheme(parseTheme(n.params.theme));
        return;
      }
```

5. 初始化时拉取权威值（在 `initialize` 成功后的那段 async 里）：

```tsx
      const settings = await client.request<{ theme?: unknown }>("ui/settings/read", {});
      setTheme(parseTheme(settings.theme));
```

6. 主题变更回调：

```tsx
  const changeTheme = (next: Theme) => {
    // 立即生效，再落盘；写失败由服务端通知/下次读取纠正。
    setTheme(next);
    clientRef.current?.request("ui/settings/write", { theme: next }).catch((e) => {
      setCurrentError(formatError(e));
      force((v) => v + 1);
    });
  };
```

7. 渲染：给 `ThreadSidebar` 传 `onOpenSettings={() => setSettingsOpen(true)}`，并在根容器内渲染：

```tsx
      <SettingsDialog
        open={settingsOpen}
        theme={theme}
        onThemeChange={changeTheme}
        onClose={() => setSettingsOpen(false)}
      />
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run && npx tsc --noEmit && npm run build`
Expected: 全绿。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/App.tsx desktop/src/App.test.tsx
git commit -m "feat(desktop): wire the settings dialog and live theme updates

Startup reads the authoritative theme, the dialog writes it, and a
ui/settings/updated notification — emitted when the agent calls set_theme —
switches the UI."
```

---

## Task 11: 文档与进度登记

**Files:**
- Modify: `docs/project-management/desktop.md`
- Modify: `docs/project-management/README.md`

- [ ] **Step 1: 登记功能**

在 `docs/project-management/desktop.md` 的 Features 列表加两条（带可验证判据，遵循该文件既有格式）：

```markdown
- [x] 设置界面（左下角入口 + 覆盖层模态 + 可扩展 Tab 栏；本次「通用」Tab）— `desktop/src/components/SettingsDialog.tsx`（`role="dialog"` + `role="tablist"`）、`desktop/src/components/SettingsGeneralTab.tsx`、入口按钮 `desktop/src/components/ThreadSidebar.tsx`（`aria-label="设置"`）；验证 `cd desktop && npx vitest run src/components/SettingsDialog.test.tsx src/components/ThreadSidebar.test.tsx`
- [x] 深色 / 浅色主题（语义色 token + 首屏防闪烁 + 对话可控）— token 层 `desktop/src/index.css`（`:root` / `[data-theme="light"]` + `@theme inline`）、`desktop/src/lib/theme.ts`、首屏脚本 `desktop/index.html`；工具与通知 `yi-agent-rs/crates/yi-agent-app-server/src/theme_tool.rs`（`SetThemeTool`）、`server.rs`（`ui/settings/read|write`、`ui/settings/updated`）、持久化 `settings_store.rs`；验证 `cd desktop && npx vitest run && npx tsc --noEmit && npm run build` + `cd yi-agent-rs && cargo test -p yi-agent-app-server`

- 已知缺陷修复：`bg-neutral-925` 非 Tailwind 色阶、此前不产生背景 → 见 Task 6，`SubagentRail.tsx` / `SuperpowersKanbanCollapsedStrip.tsx` 侧条补上底色。
```

- [ ] **Step 2: 更新索引计数**

按 `docs/project-management/README.md` 的规则，更新 desktop 行的「完成 / 总计」计数（把新增的两条计入），并核对与 `desktop.md` 条目数一致。

- [ ] **Step 3: 提交**

```bash
git add docs/project-management/desktop.md docs/project-management/README.md
git commit -m "docs: record the desktop settings dialog and theme feature"
```

---

## 完成前总验证

```bash
cd desktop && npx vitest run && npx tsc --noEmit && npm run build
cd ../yi-agent-rs && cargo fmt --all --check && cargo test -p yi-agent-app-server && cargo test -p yi-agent-core
```

全部通过后按 `superpowers:finishing-a-development-branch` 合并回 `main`（`git merge --no-ff feat/desktop-settings-theme`），并移除 worktree。
