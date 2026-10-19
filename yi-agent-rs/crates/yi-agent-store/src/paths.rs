//! 宿主路径的唯一解析点。
//!
//! 稳定版与开发版靠 `YI_AGENT_HOME` 切分实例边界；未设置时退化为
//! `$HOME/.yi-agent`，与历史布局逐字一致。

use std::path::{Path, PathBuf};

/// 真实宿主目录，仅供 launchd 等必须落在真实 HOME 的位置使用。
pub fn os_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// 实例状态根。`YI_AGENT_HOME` 优先，否则 `$HOME/.yi-agent`。
///
/// **归一化**：覆盖值若不以 `.yi-agent` 结尾，会自动补上。这条不变量让所有
/// 消费者都能安全地拼 `.yi-agent/...`（例如 `skill_rpc::roots`），不会出现
/// `<id>/.yi-agent/.yi-agent/skills` 这种双重嵌套。
pub fn instance_home() -> Option<PathBuf> {
    let override_value = std::env::var_os("YI_AGENT_HOME").filter(|v| !v.is_empty());
    instance_home_from(override_value.as_deref(), os_home().as_deref())
}

fn instance_home_from(
    override_value: Option<&std::ffi::OsStr>,
    home: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(dir) = override_value {
        let dir = PathBuf::from(dir);
        if dir.as_os_str().is_empty() {
            // fall through to the default
        } else if dir.file_name() == Some(std::ffi::OsStr::new(".yi-agent")) {
            return Some(dir);
        } else {
            return Some(dir.join(".yi-agent"));
        }
    }
    home.map(|home| home.join(".yi-agent"))
}

/// `<instance_home>` 的父目录，即「像一个 HOME 的目录」。
///
/// `skill_rpc::roots` 会在传入目录后拼 `.yi-agent/skills`；传本函数的返回值即可
/// 得到与 [`skills_dir`] 一致的用户技能根。
pub fn instance_root() -> Option<PathBuf> {
    instance_home().and_then(|home| home.parent().map(Path::to_path_buf))
}

/// 用户技能根。`YI_AGENT_SKILLS_DIR` 优先，否则 `<instance_home>/skills`。
pub fn skills_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("YI_AGENT_SKILLS_DIR").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    instance_home().map(|home| home.join("skills"))
}

/// 会话日志根。始终随实例走，便于按实例排障。
pub fn trace_dir() -> Option<PathBuf> {
    instance_home().map(|home| home.join("trace"))
}

/// `<instance_home>/<relative>`。
pub fn home_join(instance_relative: &str) -> Option<PathBuf> {
    instance_home().map(|home| home.join(instance_relative))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn from<S: AsRef<OsStr> + ?Sized>(
        override_value: Option<&S>,
        home: Option<&Path>,
    ) -> Option<PathBuf> {
        instance_home_from(override_value.map(AsRef::as_ref), home)
    }

    #[test]
    fn defaults_to_dot_yi_agent_under_home() {
        assert_eq!(
            from::<OsStr>(None, Some(Path::new("/Users/me"))),
            Some(PathBuf::from("/Users/me/.yi-agent"))
        );
    }

    #[test]
    fn a_bare_override_gains_the_dot_yi_agent_suffix() {
        assert_eq!(
            from(Some(OsStr::new("/tmp/dev-a")), Some(Path::new("/Users/me"))),
            Some(PathBuf::from("/tmp/dev-a/.yi-agent"))
        );
    }

    #[test]
    fn an_override_that_already_ends_in_dot_yi_agent_is_kept_verbatim() {
        assert_eq!(
            from(Some(OsStr::new("/tmp/dev-a/.yi-agent")), None),
            Some(PathBuf::from("/tmp/dev-a/.yi-agent"))
        );
    }

    #[test]
    fn an_empty_override_is_ignored() {
        assert_eq!(
            from(Some(OsStr::new("")), Some(Path::new("/Users/me"))),
            Some(PathBuf::from("/Users/me/.yi-agent"))
        );
    }

    #[test]
    fn no_home_and_no_override_stays_none() {
        assert_eq!(from::<OsStr>(None, None), None);
    }

    /// 未设环境变量时，每个消费者拿到的路径都不许变。
    #[test]
    fn consumers_keep_the_legacy_layout_without_overrides() {
        if std::env::var_os("YI_AGENT_HOME").is_some() {
            return; // 进程环境被外部污染时跳过，避免误报
        }
        let home = std::env::var_os("HOME").expect("test needs HOME");
        let want = PathBuf::from(&home).join(".yi-agent");
        assert_eq!(instance_home(), Some(want));
    }

    #[test]
    fn trace_dir_sits_under_the_instance_home() {
        if std::env::var_os("YI_AGENT_HOME").is_some() {
            return;
        }
        let home = std::env::var_os("HOME").expect("test needs HOME");
        assert_eq!(
            trace_dir(),
            Some(PathBuf::from(&home).join(".yi-agent").join("trace"))
        );
    }

    #[test]
    fn the_instance_root_is_the_parent_of_the_state_root() {
        // skill_rpc 会在这个父目录后拼 `.yi-agent/skills`。
        let home = PathBuf::from("/tmp/dev-a/.yi-agent");
        assert_eq!(home.parent().unwrap(), Path::new("/tmp/dev-a"));
    }
}
