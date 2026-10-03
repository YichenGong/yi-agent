//! 主题句柄与「对话切主题」工具。
//!
//! 前端才是主题的最终渲染方，agent 够不到它；工具只写共享偏好并广播，
//! 由 app-server 的 `ui/settings/updated` 通知把新值推回桌面端。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
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
        *self.theme.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// 该句柄落盘所在的目录（构造时传入的 workdir）。
    ///
    /// `ui/settings/read|write` 用它读写与 `theme` 同处
    /// `<workdir>/.yi-agent/preferences.json` 的其它宿主级偏好（如
    /// `board_watchman_enabled`）——两处若各取各的目录，就会各写一个文件。
    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    /// 更新内存值、落盘、广播。落盘失败只记日志：UI 已切了主题，不该因为
    /// 磁盘问题回退，下一次 `ui/settings/write` 会再试。
    pub fn set(&self, theme: Theme) {
        *self.theme.lock().unwrap_or_else(|p| p.into_inner()) = theme;
        if let Err(error) = settings_store::save(&self.workdir, theme) {
            tracing::warn!(%error, "failed to persist the theme preference");
        }
        let _ = self.tx.send(theme);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Theme> {
        self.tx.subscribe()
    }
}

/// 「对话切主题」工具：validate 参数后交给 [`ThemeHandle`] 落盘并广播。
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
