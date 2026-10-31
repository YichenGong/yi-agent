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
    fn install_dir_is_the_parent_of_the_binary() {
        assert_eq!(
            remote_install_dir("/home/dev/.yi-agent-projects/p/.yi-agent", "0.1.10"),
            "/home/dev/.yi-agent-projects/p/.yi-agent/bin/0.1.10"
        );
    }
}
