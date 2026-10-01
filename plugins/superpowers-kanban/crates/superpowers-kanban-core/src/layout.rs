//! 状态目录与宿主 `Layout` 的路径关系。
//!
//! 宿主的 `Layout::for_workdir` 固定为：
//! - `state_dir`   = `<workdir>/.yi-agent/superpowers-kanban`
//! - `runtime_dir` = `<workdir>/.yi-agent/runtime`
//! - 项目层偏好     = `<workdir>/.yi-agent/preferences.json`
//!
//! 插件只拿到 `state_dir`，其余都要推导出来。把推导集中在这里，是为了让
//! 「写开关的位置」和「报告项目根的位置」不可能各自漂移。

use std::path::{Path, PathBuf};

/// 项目层偏好路径：`<workdir>/.yi-agent/preferences.json`。
pub fn project_preferences_path(state_dir: &Path) -> PathBuf {
    state_dir
        .parent()
        .unwrap_or(state_dir)
        .join("preferences.json")
}

/// 项目根：`<state_dir>` 的上上级（`…/.yi-agent/superpowers-kanban` → `…`）。
///
/// 取的是 `state_dir` 的**祖父**目录而不是父目录：父目录是 `.yi-agent` 本身，
/// 那是配置目录，不是项目根。
pub fn project_root(state_dir: &Path) -> PathBuf {
    state_dir
        .parent()
        .and_then(Path::parent)
        .unwrap_or(state_dir)
        .to_path_buf()
}

/// 全局层偏好路径：`$HOME/.yi-agent/preferences.json`。无 `HOME` 时返回 `None`。
pub fn global_preferences_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| {
        PathBuf::from(home)
            .join(".yi-agent")
            .join("preferences.json")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_the_project_root_from_the_state_directory() {
        assert_eq!(
            project_root(Path::new("/proj/.yi-agent/superpowers-kanban")),
            PathBuf::from("/proj")
        );
    }

    #[test]
    fn the_project_root_is_not_the_dot_yi_agent_directory() {
        // 这是曾经写错的地方：父目录是 `.yi-agent`，不是项目根。
        let state_dir = Path::new("/proj/.yi-agent/superpowers-kanban");
        assert_ne!(project_root(state_dir), PathBuf::from("/proj/.yi-agent"));
    }

    #[test]
    fn project_preferences_live_beside_the_state_directory() {
        assert_eq!(
            project_preferences_path(Path::new("/proj/.yi-agent/superpowers-kanban")),
            PathBuf::from("/proj/.yi-agent/preferences.json")
        );
    }

    #[test]
    fn a_shallow_state_directory_does_not_panic() {
        // 兜底分支：不 panic 即可。
        assert_eq!(project_root(Path::new("state")), PathBuf::from("state"));
    }
}
