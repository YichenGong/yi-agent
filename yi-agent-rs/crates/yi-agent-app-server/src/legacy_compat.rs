//! 看板时代留下的**必要兼容面**，集中在一处、逐项记账。
//!
//! 宿主已不认识任何具体插件（spec §5.1）：通用通道、通用常驻登记、通用值守。
//! 但**磁盘与系统上仍有既成事实**，删掉它们不是「清理」而是「破坏老环境」：
//!
//! | 兼容项 | 为什么不能删 |
//! |---|---|
//! | [`LEGACY_BOARD_REGISTRY`] 迁移 | 老机器上 `boards.json` 里的项目是**用户的数据**；不迁移就等于升级后看板凭空消失（验收 §6.6） |
//! | 旧 plist 标签的清理 | 不清理会在老机器上**同时跑两个值守**（见 [`yi_agent_supervisors::watchman::LEGACY_LABEL`]） |
//! | 旧偏好键的读取 | 老机器上写下的「关掉登录自启」必须仍生效，否则开关静默复位（见 [`crate::settings_store::LEGACY_WATCHMAN_KEY`]） |
//! | [`crate::plugin_worktree`] 的命名空间 | 幽灵组长的过滤与 worktree 回收的判据都靠它（真机已有 9 例幽灵组长） |
//!
//! 每一项都必须活到「所有老环境都升级过一轮」为止。本模块把它们的**路径来源**
//! 收成单一函数，好让「宿主只在一个地方知道那个名字」这个说法可被检查：
//! 调用点引用常量，不再各自拼字符串。

use std::path::{Path, PathBuf};

/// 看板时代的登记表文件名。**旧格式的唯一载体**，只读兼容，不再写入。
pub const LEGACY_BOARD_REGISTRY: &str = "boards.json";

/// 迁移时写进通用登记表的请求者名。
///
/// **磁盘契约**：`resident-daemons.json` 里 `required_by` 存的就是这个名字，插件
/// 此后按它认领「自己要常驻的项目」。迁移时写成别的名字，插件就认不出这些项目，
/// 下一次读取会当成别人的要求而跳过——老环境照旧丢看板。故这个名字必须与插件
/// 侧读写的一致（插件 `service.rs` 的 `REQUESTER`）。
pub const SUPERPOWERS_KANBAN_REQUESTER: &str = "superpowers-kanban";

/// 旧登记表所在目录（`$HOME/.yi-agent/superpowers-kanban`）。
///
/// 迁移的**目录来源**。它以前来自一个看板 registry crate 的 `global_dir()`——那个 crate
/// 被删后若只删依赖而不换来源，迁移会静默找不到自己的文件，老环境的看板就此
/// 丢失（Task 6 必须补的那一刀）。这里就地实现同一条规则，且与那个 crate 的
/// 行为逐字一致（`$HOME` → `.yi-agent` → `superpowers-kanban`）。
///
/// `HOME` 未设置时返回 `None` 而不是猜一个目录：往凭空的路径上写文件比失败更糟
/// （与 [`yi_agent_store::resident::default_dir`] 同一策略）。
pub fn legacy_board_dir() -> Option<PathBuf> {
    legacy_board_dir_from(std::env::var_os("HOME").as_deref())
}

/// [`legacy_board_dir`] 的注入版：测试给定 `HOME`，不碰真实环境。
pub fn legacy_board_dir_from(home: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    home.map(|home| {
        PathBuf::from(home)
            .join(".yi-agent")
            .join("superpowers-kanban")
    })
}

/// 旧登记表的完整路径：`<legacy_board_dir>/boards.json`。`HOME` 缺失时 `None`。
pub fn legacy_board_registry_path() -> Option<PathBuf> {
    legacy_board_dir().map(|dir| legacy_board_registry_path_of(&dir))
}

/// [`legacy_board_registry_path`] 的注入版（目录直接给定），也是启动路径的
/// **唯一**拼装点：`$HOME` 解析与文件名拼装都收在这里，迁移那一侧不再自己拼。
pub fn legacy_board_registry_path_of(dir: &Path) -> PathBuf {
    dir.join(LEGACY_BOARD_REGISTRY)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    /// 目录来源必须与那个已删 crate 的 `global_dir()` 逐字一致，否则迁移会在
    /// 一个**空的**目录里找旧文件，老环境的看板静默丢失。
    #[test]
    fn the_legacy_dir_matches_what_the_deleted_crate_used() {
        assert_eq!(
            legacy_board_dir_from(Some(OsStr::new("/home/tester"))),
            Some(PathBuf::from("/home/tester/.yi-agent/superpowers-kanban"))
        );
    }

    /// `HOME` 缺失时不给回退目录：宁可不迁移，也不往凭空的路径上写。
    #[test]
    fn a_missing_home_yields_no_dir_rather_than_a_guess() {
        assert_eq!(legacy_board_dir_from(None), None);
    }

    #[test]
    fn the_registry_path_is_the_dir_plus_the_old_file_name() {
        assert_eq!(
            legacy_board_registry_path_of(Path::new("/h/.yi-agent/superpowers-kanban")),
            PathBuf::from("/h/.yi-agent/superpowers-kanban/boards.json")
        );
        assert_eq!(LEGACY_BOARD_REGISTRY, "boards.json");
    }
}
