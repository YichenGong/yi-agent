//! 跨进程互斥。
//!
//! # 为什么不是进程内 `Mutex`
//!
//! 同一台机器上可能有多个宿主同时管理插件：桌面壳、CLI、以及一个正在
//! 升级的实例。`Mutex` 只挡得住同进程的线程，两个进程会各自认为自己
//! 有独占权，然后同时改写同一份 registry。所以这里用 `flock`。
//!
//! # 为什么还要一张进程内表
//!
//! `flock` 按**打开文件描述**加锁，不按路径。同一进程里两次 `open` 同一
//! 路径会得到两个描述符，两次 `LOCK_EX | LOCK_NB` 都会成功——于是
//! 「本进程已持有」会被自己判成空闲。这不是理论问题：一个调用方在
//! 事件循环里嵌套两次取锁，就会得到两个"锁"，第二个并不互斥。
//!
//! 因此同路径的锁在进程内去重，返回同一个 `Arc`。表里存 `Weak`，
//! 否则表与锁互相持有，锁永不释放。
//!
//! # 锁文件永不删除
//!
//! 删掉会让「已打开同一 inode 的竞争者」锁在 unlinked 文件上，第三个
//! 进程新建同名文件后两者都以为持锁。

use std::collections::HashMap;
use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use crate::error::PluginError;

/// 持有的独占锁。`Drop` 时关闭文件，内核随之释放 `flock`。
///
/// 手写 `Debug`：`File` 的 `Debug` 会打出描述符号，那对诊断没帮助，
/// 而派生 `Debug` 会让持锁结构体没法被 `#[derive(Debug)]`。
pub struct ExclusiveLock {
    /// 只为持有 `flock` 而存在；从不读取。
    _file: File,
    path: PathBuf,
}

impl std::fmt::Debug for ExclusiveLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExclusiveLock")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl ExclusiveLock {
    /// 锁的路径，用于错误信息。
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ExclusiveLock {
    fn drop(&mut self) {
        // 从进程级表里移除自己，下一次 acquire 才会重新走内核 flock。
        // 只在该表项确已失效时删：同路径可能已被另一份 `Arc` 重新插入。
        if let Some(table) = PROCESS_LOCKS.get()
            && let Ok(mut table) = table.lock()
        {
            let stale = table
                .get(&self.path)
                .map(|w| w.strong_count() == 0)
                .unwrap_or(true);
            if stale {
                table.remove(&self.path);
            }
        }
        // `_file` 随后 drop，内核锁释放。
    }
}

type LockTable = Mutex<HashMap<PathBuf, Weak<ExclusiveLock>>>;

static PROCESS_LOCKS: OnceLock<LockTable> = OnceLock::new();

fn process_locks() -> &'static LockTable {
    PROCESS_LOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 非阻塞取锁。已被占用立即返回 [`PluginError::Busy`]。
///
/// 进程内幂等：同一路径已由本进程持有时返回同一个 `Arc`。
pub fn try_acquire(path: &Path) -> Result<Arc<ExclusiveLock>, PluginError> {
    if let Some(existing) = existing_in_process(path) {
        return Ok(existing);
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)?;

    let outcome = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if outcome != 0 {
        let error = std::io::Error::last_os_error();
        let would_block = matches!(
            error.raw_os_error(),
            Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN
        );
        if would_block {
            return Err(PluginError::Busy(format!(
                "another process holds {}",
                path.display()
            )));
        }
        // 真故障（坏描述符、不支持 flock 的文件系统）不能被报成「占用中」，
        // 否则用户永远查不出原因。
        return Err(PluginError::Io(format!(
            "could not lock {}: {error}",
            path.display()
        )));
    }

    let lock = Arc::new(ExclusiveLock {
        _file: file,
        path: path.to_path_buf(),
    });
    remember_in_process(&lock);
    Ok(lock)
}

/// 阻塞取锁，超过 `timeout` 返回 [`PluginError::Busy`]。
///
/// 用轮询而不是阻塞版 `flock`：阻塞版在拿到之前无法响应取消，而这里的
/// 超时是宿主可控的边界。
pub fn acquire(path: &Path, timeout: Duration) -> Result<Arc<ExclusiveLock>, PluginError> {
    let deadline = Instant::now() + timeout;
    loop {
        match try_acquire(path) {
            Ok(lock) => return Ok(lock),
            Err(PluginError::Busy(_)) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
    }
}

/// 当前进程内登记的锁条目数。仅供测试断言「失效条目会被清理」。
///
/// 刻意 `doc(hidden)`：这不是 API，只是让一条本不可观测的内存属性
/// 可被测试钉住，否则「表随路径数无限增长」只能靠读代码发现。
#[doc(hidden)]
pub fn tracked_lock_count() -> usize {
    let table = process_locks().lock().unwrap_or_else(|e| e.into_inner());
    table.len()
}

fn existing_in_process(path: &Path) -> Option<Arc<ExclusiveLock>> {
    let mut table = process_locks().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = table.get(path).and_then(Weak::upgrade) {
        return Some(existing);
    }
    // 这里再淘汰一次是纵深防御，**测试无法单独打红它**：`Drop` 已经在
    // 释放时清过条目，所以走到这里时表里通常根本没有这一项。留着它是因为
    // 若将来有人改 `Drop`（比如为了性能不再回收），表也不至于只增不减。
    table.remove(path);
    None
}

fn remember_in_process(lock: &Arc<ExclusiveLock>) {
    let mut table = process_locks().lock().unwrap_or_else(|e| e.into_inner());
    table.insert(lock.path.clone(), Arc::downgrade(lock));
}

// ---------------------------------------------------------------------------
// 两类锁的路径约定
//
// 两者路径不同，因此可以在持有 registry 短锁时再去取生命周期锁而不自锁
// （见 `docs/superpowers/specs/2026-11-02-plugin-distribution-lifecycle.md`
// 的锁顺序表）。反过来——持有生命周期锁、再取 registry 短锁——是允许的。
// **不允许**在持有 registry 短锁期间做网络或等待外部进程：短锁的语义就是
// 「只包住一次读改写」。
// ---------------------------------------------------------------------------

/// 插件生命周期独占锁。长期持有：从开始切换版本到切换完成。
pub fn lifecycle_lock(home: &Path) -> PathBuf {
    home.join("plugins").join("lifecycle.lock")
}

/// registry 短锁。只包住一次读改写，不跨 IO 等待。
pub fn registry_lock(home: &Path) -> PathBuf {
    home.join("plugins").join("registry.lock")
}
