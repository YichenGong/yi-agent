//! 宿主业务 daemon 的 socket 地址解析。
//!
//! 宿主把业务 daemon 的 socket 放在
//! `<instance_home>/sockets/<canonical runtime 的 SHA-256 前 16 hex>/runtime.sock`，
//! 于是地址长度只取决于实例家目录，与项目深度无关。插件此前按「runtime 目录旁
//! 的 runtime.sock，太长就回退到 TMPDIR」解析，宿主换成新布局后两者再也不会
//! 指向同一个文件——探活必然 connection refused，插件三次失败后退出。
//!
//! 本模块是宿主 `yi-agent-lifecycle::scope` 的**独立复刻**：插件不得依赖任何
//! `yi-agent-*` crate，否则它无法独立安装。两侧用同一份公共夹具
//! `plugins/contracts/fixtures/daemon-socket.json` 与跨进程真实函数对照锁死，
//! 任何一侧改了规则而没同步都会被测试拦住。
//!
//! 查询通道（`plugin-` 前缀、TMPDIR 回退）是另一套规则，原样保留在
//! [`crate::socket`]。

use std::ffi::OsStr;
use std::io;
use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

/// `sun_path` 上限（不含结尾 NUL）。与宿主 `MAX_SOCKET_PATH_BYTES` 同值。
pub const MAX_SOCKET_PATH_BYTES: usize = 103;

/// 业务 socket 解析失败。
///
/// 与查询通道的 [`crate::socket::SocketPathError`] 刻意分开：查询地址超长会
/// 回退到 TMPDIR，业务地址超长**没有回退**（回退会让插件与宿主又落到不同文件），
/// 必须让调用方明确报错。
#[derive(Debug)]
pub enum DaemonSocketError {
    /// `YI_AGENT_INSTANCE_HOME`、`YI_AGENT_HOME`、`HOME` 都不可用。
    InstanceHomeUnavailable,
    /// 候选地址仍超过 [`MAX_SOCKET_PATH_BYTES`]。
    PathTooLong {
        bytes: usize,
        limit: usize,
        path: PathBuf,
    },
}

impl std::fmt::Display for DaemonSocketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DaemonSocketError::InstanceHomeUnavailable => write!(
                f,
                "cannot resolve the instance home: none of YI_AGENT_INSTANCE_HOME, \
                 YI_AGENT_HOME or HOME is set to a non-empty value"
            ),
            DaemonSocketError::PathTooLong { bytes, limit, path } => write!(
                f,
                "daemon socket path is {bytes} bytes, over the {limit}-byte limit: {}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for DaemonSocketError {}

fn nonempty(value: Option<&OsStr>) -> Option<&OsStr> {
    value.filter(|value| !value.is_empty())
}

/// 按与宿主一致的优先级把三个环境来源解析成实例家目录。
///
/// 优先级：非空 `YI_AGENT_INSTANCE_HOME`（原样）→ 非空 `YI_AGENT_HOME`
/// （末级不是 `.yi-agent` 时追加）→ 非空 `HOME/.yi-agent` → 报错。
/// 传入纯值而不是读进程环境，测试才能在不 `set_var` 的情况下覆盖所有分支。
pub fn instance_home_from_values(
    instance: Option<&OsStr>,
    override_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Result<PathBuf, DaemonSocketError> {
    if let Some(value) = nonempty(instance) {
        return Ok(PathBuf::from(value));
    }
    if let Some(value) = nonempty(override_home) {
        let path = PathBuf::from(value);
        return Ok(if path.file_name() == Some(OsStr::new(".yi-agent")) {
            path
        } else {
            path.join(".yi-agent")
        });
    }
    nonempty(home)
        .map(|home| PathBuf::from(home).join(".yi-agent"))
        .ok_or(DaemonSocketError::InstanceHomeUnavailable)
}

/// 给定 runtime 与实例家目录，算出宿主业务 daemon 的 socket 地址。
pub fn daemon_socket_with_home(runtime: &Path, home: &Path) -> Result<PathBuf, DaemonSocketError> {
    let directory = canonical_or_self(home)
        .join("sockets")
        .join(digest16(runtime));
    let candidate = directory.join("runtime.sock");
    let bytes = candidate.as_os_str().as_encoded_bytes().len();
    if bytes > MAX_SOCKET_PATH_BYTES {
        return Err(DaemonSocketError::PathTooLong {
            bytes,
            limit: MAX_SOCKET_PATH_BYTES,
            path: candidate,
        });
    }
    Ok(candidate)
}

/// 生产包装：读取三个环境变量后委托纯函数，不写任何全局状态。
pub fn daemon_socket_for(runtime: &Path) -> Result<PathBuf, DaemonSocketError> {
    let instance = std::env::var_os("YI_AGENT_INSTANCE_HOME");
    let override_home = std::env::var_os("YI_AGENT_HOME");
    let home = std::env::var_os("HOME");
    let home = instance_home_from_values(
        instance.as_deref(),
        override_home.as_deref(),
        home.as_deref(),
    )?;
    daemon_socket_with_home(runtime, &home)
}

/// 十六位十六进制：canonical runtime 路径原始字节的 SHA-256 前 8 字节。
fn digest16(runtime: &Path) -> String {
    let digest = Sha256::digest(canonical_or_self(runtime).as_os_str().as_encoded_bytes());
    digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// 能 canonicalize 就用它，否则用原路径（首次启动时目录还不存在）。
fn canonical_or_self(path: &Path) -> PathBuf {
    canonicalize_allowing_missing(path).unwrap_or_else(|_| path.to_path_buf())
}

/// 与宿主 `scope::canonicalize_allowing_missing` 逐字一致。
///
/// 先绝对化并做词法点段处理，再尝试整路径 canonicalize；失败时找到最深的已存在
/// 祖先、canonicalize 它、把（已词法处理的）尾部重新拼回去。顺序很重要：先
/// `..` 再 canonicalize 会把 `link/..` 解析成 link 的真实父目录而不是字面父目录。
pub fn canonicalize_allowing_missing(path: &Path) -> io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        normalise(path)
    } else {
        normalise(&std::env::current_dir()?.join(path))
    };

    if let Ok(canonical) = std::fs::canonicalize(&absolute) {
        return Ok(canonical);
    }

    let mut existing: &Path = &absolute;
    let mut remainder: Vec<&OsStr> = Vec::new();
    while let Some(parent) = existing.parent() {
        if let Some(name) = existing.file_name() {
            remainder.push(name);
        }
        if parent.as_os_str().is_empty() {
            break;
        }
        if parent.exists() {
            let mut resolved = std::fs::canonicalize(parent)?;
            for component in remainder.iter().rev() {
                resolved.push(component);
            }
            return Ok(resolved);
        }
        existing = parent;
    }
    Ok(absolute)
}

fn normalise(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_values_are_skipped_in_priority_order() {
        assert_eq!(
            instance_home_from_values(
                Some(OsStr::new("")),
                Some(OsStr::new("/o")),
                Some(OsStr::new("/h"))
            )
            .unwrap(),
            PathBuf::from("/o/.yi-agent")
        );
        assert!(matches!(
            instance_home_from_values(Some(OsStr::new("")), None, None),
            Err(DaemonSocketError::InstanceHomeUnavailable)
        ));
    }

    #[test]
    fn an_instance_home_is_never_suffixed() {
        assert_eq!(
            instance_home_from_values(Some(OsStr::new("/i")), None, None).unwrap(),
            PathBuf::from("/i")
        );
    }

    #[test]
    fn the_digest_uses_canonical_bytes() {
        let root = tempfile::Builder::new()
            .prefix("kd")
            .tempdir_in("/tmp")
            .unwrap();
        let canonical = std::fs::canonicalize(root.path()).unwrap();
        assert_eq!(digest16(&canonical), digest16(root.path()));
    }
}
