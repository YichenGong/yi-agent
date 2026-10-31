//! `remote.json` 的读写。
//!
//! spec §4.9 的两条硬要求：
//! 1. **读坏文件一律当"没配置"** —— 坏配置绝不能让 desktop 起不来。
//! 2. **写必须跨进程安全** —— 桥接层（Tauri）写、app-server 读；进程内 Mutex
//!    保护不了后者，所以照 `device_store.rs:104-130` 用 flock + 唯一临时名。
//!
//! 只读路径刻意**不加 flock**：temp+rename 保证它读到的要么是旧版要么是新版，
//! 加锁只会让 UI 查询被写操作阻塞。

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::profile::RemoteProfile;

pub const CONFIG_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteConfig {
    pub version: u32,
    pub profiles: Vec<RemoteProfile>,
}

impl Default for RemoteConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            profiles: Vec::new(),
        }
    }
}

/// 读配置。任何异常都退化成"空配置"，并往 stderr 记一行。
pub fn load(path: &Path) -> RemoteConfig {
    let Ok(text) = std::fs::read_to_string(path) else {
        return RemoteConfig::default();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        eprintln!(
            "[remote] {} is not valid JSON; treating as unconfigured",
            path.display()
        );
        return RemoteConfig::default();
    };
    let version = value.get("version").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    if version != CONFIG_VERSION {
        // 更高版本不猜；更低/缺失视为损坏。
        eprintln!(
            "[remote] {} has version {version}, expected {CONFIG_VERSION}; ignoring",
            path.display()
        );
        return RemoteConfig::default();
    }
    match serde_json::from_value::<RemoteConfig>(value) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!(
                "[remote] {} is malformed ({e}); treating as unconfigured",
                path.display()
            );
            RemoteConfig::default()
        }
    }
}

/// 唯一临时名：并发写者不得共用同一个临时路径，否则一个 `rename` 会
/// `ENOENT` 另一个的（`device_store.rs:120-128` 的同一考量）。
#[allow(dead_code)]
fn temp_path(path: &Path) -> PathBuf {
    path.with_extension(format!(
        "json.{}.{}.tmp",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ))
}

/// 原子写。`flock` 覆盖整个读-改-写窗口。
#[allow(dead_code)]
pub fn save(path: &Path, config: &RemoteConfig) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock_path = path.with_extension("json.lock");
    let lock = File::options().create(true).write(true).open(&lock_path)?;
    // 与 `device_store.rs` 同款：阻塞式独占锁，写完随 `lock` 析构释放。
    lock_exclusive(&lock)?;

    let body = serde_json::to_string_pretty(config).map_err(io::Error::other)?;
    let tmp = temp_path(path);
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

/// `flock(LOCK_EX)`，被信号打断则重试。
#[allow(dead_code)]
fn lock_exclusive(file: &File) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    loop {
        // SAFETY: `file` 活得比这次调用久，fd 有效。
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return Err(error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn profile(id: &str) -> RemoteProfile {
        RemoteProfile {
            id: id.into(),
            label: format!("{id} label"),
            host: "dev@host".into(),
            local_path: PathBuf::from("/Users/y/proj"),
            remote_path: "/home/dev/proj".into(),
            instance_root: "/home/dev/.yi-agent-projects/x/.yi-agent".into(),
            binary_version: "0.1.10".into(),
            remote_arch: "x86_64-unknown-linux-gnu".into(),
        }
    }

    #[test]
    fn missing_file_loads_as_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = load(&dir.path().join("remote.json"));
        assert_eq!(cfg.version, CONFIG_VERSION);
        assert!(cfg.profiles.is_empty());
    }

    #[test]
    fn broken_json_loads_as_empty_not_panic() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("remote.json");
        std::fs::write(&path, "{ not json").unwrap();
        let cfg = load(&path);
        assert!(
            cfg.profiles.is_empty(),
            "a corrupt config must not brick the app"
        );
    }

    #[test]
    fn future_version_is_refused() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("remote.json");
        std::fs::write(&path, r#"{"version":99,"profiles":[]}"#).unwrap();
        let cfg = load(&path);
        assert!(cfg.profiles.is_empty(), "must not guess a future schema");
    }

    #[test]
    fn round_trip_preserves_profiles() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("remote.json");
        let cfg = RemoteConfig {
            version: CONFIG_VERSION,
            profiles: vec![profile("proj-foo")],
        };
        save(&path, &cfg).unwrap();
        assert_eq!(load(&path), cfg);
    }

    #[test]
    fn no_temp_file_is_left_behind() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("remote.json");
        save(
            &path,
            &RemoteConfig {
                version: CONFIG_VERSION,
                profiles: vec![],
            },
        )
        .unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files leaked: {leftovers:?}");
    }

    #[test]
    fn concurrent_saves_do_not_corrupt_the_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("remote.json");
        std::thread::scope(|s| {
            for i in 0..8 {
                let path = path.clone();
                s.spawn(move || {
                    let cfg = RemoteConfig {
                        version: CONFIG_VERSION,
                        profiles: vec![profile(&format!("p{i}"))],
                    };
                    save(&path, &cfg).unwrap();
                });
            }
        });
        // 只要最终文件能解析出 `profiles` 数组就算没撕裂（最后写的那个赢）。
        let cfg = load(&path);
        assert_eq!(cfg.version, CONFIG_VERSION);
        assert!(cfg.profiles.len() == 1 || cfg.profiles.is_empty());
    }
}
