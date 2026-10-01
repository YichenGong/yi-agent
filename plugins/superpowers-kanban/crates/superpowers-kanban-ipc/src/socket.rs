//! Socket 路径解析：让插件在深路径项目里也能通。
//!
//! `sockaddr_un.sun_path` 只有 [`MAX_SOCKET_PATH_BYTES`] 字节，深项目路径会溢出，
//! `bind` 直接失败。宿主早已有回退规则；插件此前**没有**，于是查询 socket
//! 在深路径下永远 bind 不上，看板 UI 全瞎。
//!
//! # 为什么这里有两套规则，且**不得合并**
//!
//! | 规则 | 前缀 | 哈希输入 | 谁用 |
//! |---|---|---|---|
//! | 宿主 daemon socket | `yi-agent-` | `runtime_dir` | 宿主 daemon、宿主所有客户端、**插件客户端** |
//! | 插件通道 socket | `plugin-` | **直接路径** | 宿主转发表、**插件服务端** |
//!
//! 两套规则用途不同、且必须与「另一边」逐字一致：
//!
//! * [`daemon_socket_for`] 复刻宿主 `yi-agent-store::ipc::socket_path_for`，
//!   因为插件要连的是**宿主**的 daemon。哈希输入是 `runtime_dir`。
//! * [`plugin_socket_for`] 是插件与宿主转发表之间的约定，哈希输入是
//!   **展开后的直接路径**——宿主拿到清单值、插件拿到 `dir.join(file_name)`，
//!   当清单写 `{state_dir}/superpowers-kanban.sock` 时两者逐字相同，
//!   于是哈希相同、结果相同。
//!
//! 前缀不同是有意的：它让两个通道的 socket 永远不会撞名。

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// `sun_path` 上限（不含结尾 NUL）。Linux 允许 108，macOS/BSD 允许 104；
/// 取更小的那个让行为跨平台一致。与宿主 `MAX_SOCKET_PATH_BYTES` 同值。
pub const MAX_SOCKET_PATH_BYTES: usize = 103;

/// 解析失败：回退后仍然超长（`$TMPDIR` 本身太深）。
#[derive(Debug)]
pub struct SocketPathError {
    pub path: String,
    pub limit: usize,
}

impl std::fmt::Display for SocketPathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "socket path is {} bytes after fallback, over the {}-byte limit: {}",
            self.path.len(),
            self.limit,
            self.path
        )
    }
}

impl std::error::Error for SocketPathError {}

/// 插件通道 socket：宿主转发表与插件服务端**共用**的规则。
///
/// 哈希输入是**直接路径**，两边因此能对上（见模块文档）。
pub fn plugin_socket_for(direct: &Path) -> Result<PathBuf, SocketPathError> {
    resolve(
        direct,
        direct.as_os_str().as_encoded_bytes(),
        "plugin-",
        &std::env::temp_dir(),
    )
}

/// 插件→daemon 的 socket：逐字复刻宿主 `socket_path_for`（哈希 `runtime_dir`）。
///
/// 与 [`plugin_socket_for`] 的差异是**有意的**：这条连的是宿主的 daemon，
/// 必须与宿主的命名（`yi-agent-` 前缀、哈希 `runtime_dir`）完全一致。
pub fn daemon_socket_for(runtime_dir: &Path) -> Result<PathBuf, SocketPathError> {
    let direct = runtime_dir.join("runtime.sock");
    resolve(
        &direct,
        runtime_dir.as_os_str().as_encoded_bytes(),
        "yi-agent-",
        &std::env::temp_dir(),
    )
}

/// 共用内核：直通 → 回退 → 回退仍超长则报错。
///
/// `hash_input` 与 `direct` 分开传：宿主规则哈希的是**目录**，插件通道哈希的是
/// **直接路径**。把差异集中在调用处，内核只做一件事。
fn resolve(
    direct: &Path,
    hash_input: &[u8],
    prefix: &str,
    temp_dir: &Path,
) -> Result<PathBuf, SocketPathError> {
    if direct.as_os_str().len() <= MAX_SOCKET_PATH_BYTES {
        return Ok(direct.to_path_buf());
    }

    let digest = Sha256::digest(hash_input);
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    let fallback = temp_dir.join(format!("{prefix}{}.sock", &hex[..16]));

    if fallback.as_os_str().len() > MAX_SOCKET_PATH_BYTES {
        // 静默返回一条 bind 不上的路径会让问题在下游以「插件不存在」的形式炸开，
        // 排查成本极高。这里 fail-fast。
        return Err(SocketPathError {
            path: fallback.display().to_string(),
            limit: MAX_SOCKET_PATH_BYTES,
        });
    }

    Ok(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 与宿主 `yi-agent-store::ipc` 测试里同一个深路径，用来锁住两侧一致。
    const LONG_RUNTIME_DIR: &str = "/Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.yi-agent/runtime";
    const LONG_STATE_DIR: &str = "/Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.yi-agent/superpowers-kanban";

    fn long_query_socket() -> PathBuf {
        Path::new(LONG_STATE_DIR).join("superpowers-kanban.sock")
    }

    #[test]
    fn a_short_direct_path_is_returned_unchanged() {
        let direct = Path::new("/tmp/project/.yi-agent/state/superpowers-kanban.sock");
        assert_eq!(plugin_socket_for(direct).expect("resolves"), direct);
    }

    #[test]
    fn a_long_direct_path_falls_back_under_the_temp_dir() {
        let direct = long_query_socket();
        assert!(
            direct.as_os_str().len() > MAX_SOCKET_PATH_BYTES,
            "precondition: 直接路径必须越界"
        );

        let socket = plugin_socket_for(&direct).expect("falls back");

        assert!(socket.as_os_str().len() <= MAX_SOCKET_PATH_BYTES);
        assert_eq!(socket.parent().unwrap(), std::env::temp_dir());
        assert_eq!(
            socket.file_name().unwrap(),
            "plugin-b16a6326f33a64c5.sock",
            "规则漂移了：宿主转发表会指向别的地方"
        );
    }

    #[test]
    fn the_plugin_channel_never_reuses_the_daemon_namespace() {
        let socket = plugin_socket_for(&long_query_socket()).expect("falls back");
        let name = socket.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("plugin-"), "got {name}");
        assert!(
            !name.starts_with("yi-agent-"),
            "插件通道与 daemon socket 是两个命名空间，撞名会互相覆盖"
        );
    }

    #[test]
    fn the_plugin_fallback_is_deterministic() {
        let direct = long_query_socket();
        assert_eq!(
            plugin_socket_for(&direct).expect("resolves"),
            plugin_socket_for(&direct).expect("resolves"),
            "宿主与插件必须解析出同一条路径"
        );
    }

    #[test]
    fn the_daemon_rule_matches_the_host() {
        let socket = daemon_socket_for(Path::new(LONG_RUNTIME_DIR)).expect("falls back");
        assert_eq!(socket.parent().unwrap(), std::env::temp_dir());
        assert_eq!(
            socket.file_name().unwrap(),
            "yi-agent-51bccf606c7e2ae7.sock",
            "必须与宿主 socket_path_for 逐字一致"
        );
    }

    #[test]
    fn a_short_runtime_dir_keeps_the_socket_inside_it() {
        let dir = Path::new("/tmp/project/.yi-agent/runtime");
        assert_eq!(
            daemon_socket_for(dir).expect("resolves"),
            dir.join("runtime.sock")
        );
    }

    #[test]
    fn a_temp_dir_that_is_still_too_deep_is_an_error() {
        // 现状是静默失败，排查成本高；回退不了就必须显式报错。
        let deep_temp = Path::new(LONG_STATE_DIR).join(".yi-agent/.yi-agent/.yi-agent");
        let direct = long_query_socket();
        assert!(
            deep_temp.join("plugin-0000000000000000.sock")
                .as_os_str()
                .len()
                > MAX_SOCKET_PATH_BYTES,
            "precondition: 注入的 temp dir 必须让回退也放不下"
        );

        assert!(
            resolve(&direct, direct.as_os_str().as_encoded_bytes(), "plugin-", &deep_temp)
                .is_err(),
            "回退后仍超长必须报错，不能静默返回一条 bind 不上的路径"
        );
        assert!(
            resolve(&direct, direct.as_os_str().as_encoded_bytes(), "plugin-", &std::env::temp_dir())
                .is_ok(),
            "正常 temp dir 必须成功"
        );
    }
}
