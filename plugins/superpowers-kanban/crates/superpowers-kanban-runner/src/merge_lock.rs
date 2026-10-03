//! 每项目一把合并闸：同一项目任意时刻至多一个合并在执行。
//!
//! 与 `plugin.lock` 同理由：flock 由内核在持有进程退出（含 SIGKILL）时释放，
//! PID 文件做不到。锁文件在释放时**不 unlink**，否则会出现两个 inode 各持锁。

use std::fs::{File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

pub struct MergeLock {
    /// 关闭这个描述符就是全部释放协议，所以它从不被读——只为把锁按住而保活。
    _file: File,
    path: PathBuf,
}

impl MergeLock {
    pub fn path(&self) -> &Path {
        &self.path
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
