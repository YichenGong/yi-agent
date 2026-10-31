//! 每项目一把合并闸：同一项目任意时刻至多一个合并在执行。
//!
//! 与 `plugin.lock` 同理由：flock 由内核在持有进程退出（含 SIGKILL）时释放，
//! PID 文件做不到。锁文件在释放时**不 unlink**，否则会出现两个 inode 各持锁。

use std::fs::{File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub struct MergeLock {
    /// 关闭这个描述符就是全部释放协议，所以它从不被读——只为把锁按住而保活。
    _file: File,
    path: PathBuf,
}

impl std::fmt::Debug for MergeLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MergeLock")
            .field("path", &self.path)
            .finish()
    }
}

impl MergeLock {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// 谁拿着合并名额。`flock` 的 fd 不能跨进程读内容，故另落一个小文件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holder {
    pub card_id: String,
    pub pid: u32,
    pub since: String,
}

/// 一次取名额的结局。`Reclaimed` 表示上一位持有者已经死了、记录被接管。
#[derive(Debug)]
pub enum MergeAttempt {
    Granted(MergeLock),
    /// 有人正拿着：带上它的卡号，好让人知道在等谁。
    Busy {
        card_id: String,
        pid: u32,
    },
    Reclaimed(MergeLock),
}

fn holder_path(state_dir: &Path) -> PathBuf {
    state_dir.join("merge.lock.holder")
}

/// 读 holder 记录。文件缺失或写坏了都回 `None`——记录只是**说明**，
/// 真实性由 `flock` 本身保证。
pub fn read_holder(state_dir: &Path) -> Option<Holder> {
    let text = std::fs::read_to_string(holder_path(state_dir)).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_holder(state_dir: &Path, holder: &Holder) -> std::io::Result<()> {
    let text = serde_json::to_string(holder).map_err(std::io::Error::other)?;
    let path = holder_path(state_dir);
    let tmp = path.with_extension("holder.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(tmp, path)
}

/// 删掉 holder 记录。文件本就不在也算成功。
pub fn clear_holder(state_dir: &Path) {
    let _ = std::fs::remove_file(holder_path(state_dir));
}

/// `pid` 还活着吗。`ESRCH` 才算消失；`EPERM` 视为存在（别人的进程也是活的）。
fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let outcome = unsafe { libc::kill(pid as i32, 0) };
    if outcome == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// holder 记录里的进程还活着吗。给「记录在、但没人真持有」的残留判定用。
pub fn holder_alive(holder: &Holder) -> bool {
    pid_alive(holder.pid)
}

/// 取名额并记下持有者。
///
/// 拿到 `flock` 说明没有活着的进程在合并；此时若 holder 记录里的 pid 已经消失，
/// 说明上一位持有者崩在两次写之间——记录被接管，回 `Reclaimed`。
pub fn acquire_with_holder(state_dir: &Path, card_id: &str) -> MergeAttempt {
    let previous = read_holder(state_dir);
    let lock = match acquire(state_dir) {
        Some(lock) => lock,
        None => {
            let holder = previous.unwrap_or(Holder {
                card_id: String::new(),
                pid: 0,
                since: String::new(),
            });
            return MergeAttempt::Busy {
                card_id: holder.card_id,
                pid: holder.pid,
            };
        }
    };
    let reclaimed = previous
        .as_ref()
        .is_some_and(|holder| !pid_alive(holder.pid));
    let _ = write_holder(
        state_dir,
        &Holder {
            card_id: card_id.to_string(),
            pid: std::process::id(),
            since: chrono::Local::now().to_rfc3339(),
        },
    );
    if reclaimed {
        MergeAttempt::Reclaimed(lock)
    } else {
        MergeAttempt::Granted(lock)
    }
}

/// 取 `<state_dir>/merge.lock` 的独占锁；已被占用返回 `None`。
///
/// 锁随 fd 关闭由内核释放（含 `SIGKILL`），所以**不在 drop 时 unlink**：删除
/// 文件会让另一个进程在旧 inode 上持锁、在新 inode 上再取到锁，等于没锁。
pub fn acquire(state_dir: &Path) -> Option<MergeLock> {
    std::fs::create_dir_all(state_dir).ok()?;
    let path = state_dir.join("merge.lock");
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .ok()?;
    let outcome = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if outcome == 0 {
        Some(MergeLock { _file: file, path })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dead_holder_is_reclaimed_instead_of_blocking() {
        let dir = tempfile::tempdir().unwrap();
        // pid 0 永远不会被当作存活进程。
        std::fs::write(
            holder_path(dir.path()),
            r#"{"card_id":"old","pid":0,"since":"2026-10-01T00:00:00+08:00"}"#,
        )
        .unwrap();
        match acquire_with_holder(dir.path(), "new") {
            MergeAttempt::Reclaimed(_) => {}
            MergeAttempt::Granted(_) => {}
            other => panic!("a dead holder must not block: {other:?}"),
        }
        let holder = read_holder(dir.path()).unwrap();
        assert_eq!(holder.card_id, "new");
    }

    #[test]
    fn a_live_holder_reports_busy_with_its_card() {
        let dir = tempfile::tempdir().unwrap();
        let held = match acquire_with_holder(dir.path(), "card-a") {
            MergeAttempt::Granted(lock) => lock,
            other => panic!("expected the first grant, got {other:?}"),
        };
        match acquire_with_holder(dir.path(), "card-b") {
            MergeAttempt::Busy { card_id, .. } => assert_eq!(card_id, "card-a"),
            other => panic!("expected busy, got {other:?}"),
        }
        drop(held);
    }

    #[test]
    fn clearing_the_holder_record_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        clear_holder(dir.path());
        assert!(read_holder(dir.path()).is_none());
    }

    #[test]
    fn one_project_admits_a_single_merger() {
        let dir = tempfile::tempdir().unwrap();
        let held = acquire(dir.path()).expect("first merger takes the gate");
        assert!(held.path().ends_with("merge.lock"));
        assert!(acquire(dir.path()).is_none(), "a second merger must wait");
        drop(held);
        // drop 释放经内核解 flock；有界轮询即可。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut released = false;
        while std::time::Instant::now() < deadline {
            if let Some(lock) = acquire(dir.path()) {
                drop(lock);
                released = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(released, "the gate reopens after the holder drops it");
    }
}
