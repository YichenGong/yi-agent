//! 安装位置与本机缓存的路径派生。
//!
//! 全是纯字符串/路径拼接，没有 IO——所以能穷举测试。实际写盘在计划 02 的
//! `fetch.rs`（本机下载）与 `install.rs`（远端解压，经 ssh）。

use std::path::{Path, PathBuf};

/// 本机缓存。**放在 `instance_home` 下**，跨 App 重装存活（spec §2.4.2）。
pub fn cache_path(home: &Path, version: &str, target: &str) -> PathBuf {
    home.join("remote-cache")
        .join(version)
        .join(format!("yi-agent-{target}.tar.gz"))
}

/// 远端二进制路径。按版本分目录，多版本并存不打架（spec §2.5）。
pub fn remote_binary_path(instance_root: &str, version: &str) -> String {
    format!("{instance_root}/bin/{version}/yi-agent")
}

/// 远端的版本目录，即解压目标。
pub fn remote_install_dir(instance_root: &str, version: &str) -> String {
    format!("{instance_root}/bin/{version}")
}

/// 删掉除 `keep_version` 之外的缓存目录，返回被删的路径。
///
/// 没有清理逻辑时缓存会无限增长（每次升级多十几 MB）。只删
/// `remote-cache/` 下**看起来像版本号**的目录（首字符是数字），避免误删用户
/// 自己放进来的东西。
pub fn prune(home: &Path, keep_version: &str) -> std::io::Result<Vec<PathBuf>> {
    let root = home.join("remote-cache");
    let mut removed = Vec::new();
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(removed),
        Err(error) => return Err(error),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if !path.is_dir() || name == keep_version || !name.starts_with(|c: char| c.is_ascii_digit())
        {
            continue;
        }
        std::fs::remove_dir_all(&path)?;
        removed.push(path);
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_lives_under_the_instance_home() {
        assert_eq!(
            cache_path(
                Path::new("/Users/y/.yi-agent"),
                "0.1.10",
                "x86_64-unknown-linux-gnu"
            ),
            PathBuf::from(
                "/Users/y/.yi-agent/remote-cache/0.1.10/yi-agent-x86_64-unknown-linux-gnu.tar.gz"
            )
        );
    }

    #[test]
    fn different_versions_do_not_collide() {
        let a = cache_path(Path::new("/h"), "0.1.9", "t");
        let b = cache_path(Path::new("/h"), "0.1.10", "t");
        assert_ne!(a, b);
    }

    /// 按版本分目录：升级后旧版本仍可用，回滚不需要重新下载。
    #[test]
    fn remote_binary_is_versioned_not_overwritten() {
        assert_eq!(
            remote_binary_path("/home/dev/.yi-agent-projects/p/.yi-agent", "0.1.10"),
            "/home/dev/.yi-agent-projects/p/.yi-agent/bin/0.1.10/yi-agent"
        );
    }

    #[test]
    fn prune_keeps_the_requested_version_only() {
        let dir = tempfile::TempDir::new().unwrap();
        let home = dir.path();
        for version in ["0.1.9", "0.1.10"] {
            let path = cache_path(home, version, "t");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"x").unwrap();
        }

        let removed = prune(home, "0.1.10").unwrap();

        assert_eq!(removed.len(), 1, "{removed:?}");
        assert!(cache_path(home, "0.1.10", "t").exists());
        assert!(!cache_path(home, "0.1.9", "t").exists());
    }

    #[test]
    fn prune_on_a_missing_cache_dir_is_a_noop() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(prune(dir.path(), "0.1.10").unwrap().is_empty());
    }

    /// 只删「像版本号」的目录：用户可能把别的东西放进 remote-cache。
    #[test]
    fn prune_leaves_directories_that_do_not_look_like_versions() {
        let dir = tempfile::TempDir::new().unwrap();
        let keep = dir.path().join("remote-cache").join("notes");
        std::fs::create_dir_all(&keep).unwrap();

        let removed = prune(dir.path(), "0.1.10").unwrap();

        assert!(removed.is_empty(), "{removed:?}");
        assert!(keep.exists(), "a non-version directory must survive");
    }

    #[test]
    fn install_dir_is_the_parent_of_the_binary() {
        assert_eq!(
            remote_install_dir("/home/dev/.yi-agent-projects/p/.yi-agent", "0.1.10"),
            "/home/dev/.yi-agent-projects/p/.yi-agent/bin/0.1.10"
        );
    }
}
