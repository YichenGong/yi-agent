//! 项目维护所有权。
//!
//! # 为什么不是"按时间过期"
//!
//! 一个 owner 会长时间不活动：它在等用户审批、在被 `SIGSTOP` 暂停、在
//! 等一个慢查询。任何基于 TTL 或 mtime 的回收都会把这些都判成"已退出"，
//! 于是第二个实例拿到所有权，在第一个还在写的时候迁移同一份数据。
//!
//! `flock` 的行为正好是要的：进程退出（含 `SIGKILL`）由内核释放，
//! `SIGSTOP` **不**释放。所以存活判定只能问锁，不能问时间。
//!
//! # 非阻塞失败
//!
//! 设计 §7.1 要求跨实例争用所有权时**非阻塞失败**：不能抢占活跃持有者。
//! 维护动作在拿不到所有权时应当报 `Busy` 并列出当前持有者，由用户决定
//! 是等它结束还是换一个时机。

use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::PluginError;
use crate::locks::{self, ExclusiveLock};

/// 一个项目维护所有权。
///
/// 持有期间 `flock` 住 `owners/<project>.lock`。`Drop` 关闭描述符即释放。
#[derive(Debug)]
pub struct ProjectOwnership {
    root: PathBuf,
    /// 本进程已持有的锁，按项目规范化键索引。值里带 owner 身份：同一个
    /// 进程里的两个不同 owner 也必须互相排斥，否则"同进程已持有"会让
    /// 第二个调用方拿到一个它并不拥有的 guard。
    held: std::sync::Mutex<std::collections::BTreeMap<String, HeldOwner>>,
}

/// 所有者信息，用于在 `Busy` 时告诉用户"谁拿着"。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnerRecord {
    pub project: String,
    /// 宿主/实例标识。
    pub owner: String,
    /// 进程 id，仅供显示与排查；**不能**用来判断存活。
    pub pid: u32,
}

#[derive(Debug, Clone)]
struct HeldOwner {
    owner: String,
    lock: Arc<ExclusiveLock>,
}

/// `acquire` 对「瞬时占用」的重试窗口。
///
/// 只为覆盖 `fork`/`exec` 之间的共享描述符窗口，不用于等待真实的持有者。
struct OwnershipRetry;

impl OwnershipRetry {
    const WINDOW: std::time::Duration = std::time::Duration::from_millis(250);
}

impl ProjectOwnership {
    /// `root` 是所有权目录，由宿主注入（与实例根分开）。
    pub fn new(root: impl Into<PathBuf>) -> ProjectOwnership {
        ProjectOwnership {
            root: root.into(),
            held: std::sync::Mutex::new(std::collections::BTreeMap::new()),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn lock_path(&self, project_key: &str) -> PathBuf {
        self.root.join(format!("{}.lock", sanitize(project_key)))
    }

    fn owner_path(&self, project_key: &str) -> PathBuf {
        self.root
            .join(format!("{}.owner.json", sanitize(project_key)))
    }

    /// 尝试取得项目所有权。拿不到立即 `Busy`（非阻塞）。
    pub fn acquire(
        &self,
        project_key: &str,
        owner: &str,
        pid: u32,
    ) -> Result<OwnerGuard<'_>, PluginError> {
        let path = self.lock_path(project_key);
        let key = sanitize(project_key);
        {
            let held = self.held.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(existing) = held.get(&key) {
                if existing.owner == owner {
                    // 同一个 owner 重入：返回一个不拥有锁的 guard，drop 时不能放，
                    // 否则它会把仍在使用的锁提前释放掉。
                    return Ok(OwnerGuard {
                        ownership: self,
                        project_key: key,
                        _lock: Arc::clone(&existing.lock),
                        owns_lock: false,
                    });
                }
                // 不同 owner、同一进程：锁其实被本进程占着，但那个 owner
                // 不是这个调用方。必须与非阻塞争用一样失败。
                let holder = self.read_owner(project_key).ok().flatten();
                return Err(PluginError::Busy(match holder {
                    Some(record) => format!(
                        "project `{project_key}` is owned by `{}` (pid {}); maintenance cannot \
                         preempt a live owner",
                        record.owner, record.pid
                    ),
                    None => format!(
                        "project `{project_key}` is owned by `{}` in this process; maintenance \
                         cannot preempt a live owner",
                        existing.owner
                    ),
                }));
            }
        }

        // 有界重试，而不是一次 `try_acquire` 就报 Busy。
        //
        // 原因不是"想等对手退出"（那会变成抢占），而是 `fork` 到 `exec` 之间
        // 的子进程与本进程共享同一个打开文件描述：宿主任何线程在那个窗口里
        // `spawn` 子进程，都会让本进程刚刚释放的锁短暂地仍被持有（毫秒级）。
        // 一次尝试会把这种瞬时状态报成"别的实例占着这个项目"，从而让维护
        // 动作无理由地失败。
        //
        // 重试**不会**降低互斥强度：真正的持有者仍由 `flock` 挡住，我们只是
        // 多试几次；窗口一过立刻成功，真有别的 owner 时依旧在有界时间内返回
        // `Busy`。
        let lock = locks::acquire(&path, OwnershipRetry::WINDOW).map_err(|error| match error {
            PluginError::Busy(_) => {
                let holder = self.read_owner(project_key).ok().flatten();
                PluginError::Busy(match holder {
                    Some(record) => format!(
                        "project `{project_key}` is owned by `{}` (pid {}); maintenance cannot \
                         preempt a live owner",
                        record.owner, record.pid
                    ),
                    None => format!(
                        "project `{project_key}` is owned by another process; maintenance cannot \
                         preempt a live owner"
                    ),
                })
            }
            other => other,
        })?;

        self.held.lock().unwrap_or_else(|e| e.into_inner()).insert(
            key.clone(),
            HeldOwner {
                owner: owner.to_string(),
                lock: Arc::clone(&lock),
            },
        );

        // 记录是谁拿着，只为诊断。写失败不阻断：它不影响互斥正确性。
        let _ = self.write_owner(project_key, owner, pid);

        Ok(OwnerGuard {
            ownership: self,
            project_key: key,
            _lock: lock,
            owns_lock: true,
        })
    }

    /// 这个项目现在有没有活着的 owner。
    ///
    /// 用**独立描述符**试锁：`flock` 按打开文件描述加锁，同进程第二个
    /// 描述符照样冲突，所以拿到锁 = 没人持有。
    pub fn is_alive(&self, project_key: &str) -> Result<bool, PluginError> {
        self.is_alive_at(&self.lock_path(project_key))
    }

    /// 按**已经投影好的**锁路径判断存活。
    ///
    /// `reclaimable` 从文件名里拿到的是投影后的名字，再走一次
    /// `sanitize` 会得到另一个文件名的摘要，永远查不到任何东西——
    /// 那会让每次回收都"看起来没有活 owner"。
    fn is_alive_at(&self, path: &Path) -> Result<bool, PluginError> {
        if !path.exists() {
            return Ok(false);
        }
        // 探测用独立描述符。注意 `fork` 与 `exec` 之间的子进程与本进程共享
        // 打开文件描述，所以「刚刚释放」可能短暂探测成「仍持有」。这是 flock
        // 的固有窗口（见 `locks.rs` 顶部），调用方按有界收敛处理即可。
        let Ok(probe) = std::fs::OpenOptions::new().write(true).open(path) else {
            // 打不开不能证明"没人持有"，按仍持有处理。
            return Ok(true);
        };
        let outcome = unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if outcome == 0 {
            unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_UN) };
            return Ok(false);
        }
        let error = std::io::Error::last_os_error();
        Ok(matches!(
            error.raw_os_error(),
            Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN
        ))
    }

    /// 所有**已死亡** owner 的残留记录。
    ///
    /// 只回收已死亡确认的那些：`SIGSTOP` 的进程仍然活着，它的记录必须
    /// 保留，否则下一个实例会以为可以接手。
    pub fn reclaimable(&self) -> Result<Vec<OwnerRecord>, PluginError> {
        let mut out = Vec::new();
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(error) => return Err(PluginError::Io(format!("{}: {error}", self.root.display()))),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(project) = name.strip_suffix(".owner.json") else {
                continue;
            };
            if self.is_alive_at(&self.root.join(format!("{project}.lock")))? {
                continue;
            }
            if let Ok(Some(record)) =
                self.read_owner_at(&self.root.join(format!("{project}.owner.json")))
            {
                out.push(record);
            }
        }
        out.sort_by(|a, b| a.project.cmp(&b.project));
        Ok(out)
    }

    fn read_owner(&self, project_key: &str) -> Result<Option<OwnerRecord>, PluginError> {
        self.read_owner_at(&self.owner_path(project_key))
    }

    fn read_owner_at(&self, path: &Path) -> Result<Option<OwnerRecord>, PluginError> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|error| PluginError::Io(format!("{}: {error}", path.display()))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(PluginError::Io(format!("{}: {error}", path.display()))),
        }
    }

    fn write_owner(&self, project_key: &str, owner: &str, pid: u32) -> Result<(), PluginError> {
        std::fs::create_dir_all(&self.root)?;
        let record = OwnerRecord {
            project: project_key.to_string(),
            owner: owner.to_string(),
            pid,
        };
        let bytes = serde_json::to_vec_pretty(&record)
            .map_err(|e| PluginError::Io(format!("could not serialize owner record: {e}")))?;
        crate::journal::write_atomic(&self.root, &self.owner_path(project_key), &bytes)
    }
}

/// 所有权凭据。`Drop` 时释放（仅当本 guard 真正持有那把锁）。
pub struct OwnerGuard<'a> {
    ownership: &'a ProjectOwnership,
    project_key: String,
    _lock: Arc<ExclusiveLock>,
    owns_lock: bool,
}

impl std::fmt::Debug for OwnerGuard<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnerGuard")
            .field("project", &self.project_key)
            .field("owns_lock", &self.owns_lock)
            .finish()
    }
}

impl OwnerGuard<'_> {
    pub fn project(&self) -> &str {
        &self.project_key
    }

    pub fn owns_lock(&self) -> bool {
        self.owns_lock
    }
}

impl Drop for OwnerGuard<'_> {
    fn drop(&mut self) {
        if !self.owns_lock {
            return;
        }
        self.ownership
            .held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.project_key);
        // 清掉诊断记录：留着会让下一个人看到一个已经不住在这儿的 owner。
        let _ = std::fs::remove_file(self.ownership.owner_path(&self.project_key));
        // `_lock` 随后 drop，内核释放 flock。
    }
}

/// 把项目键变成安全的文件名。
///
/// 项目键通常是规范化路径，含 `/`。直接拼进文件名会创建多层目录，
/// 而两层不同的项目可能映射到同一个文件名——那会让两个项目共用一个
/// 所有权锁。
fn sanitize(key: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(key.as_bytes());
    let hex: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
    // 保留可读前缀，便于人工排查；唯一的尾部由摘要保证。
    let mut readable: String = key
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(32)
        .collect();
    if readable.is_empty() {
        readable.push_str("project");
    }
    format!("{readable}-{hex}")
}
