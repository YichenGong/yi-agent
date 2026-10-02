//! 同一项目同一时刻只允许一个插件推进队列。
//!
//! daemon 崩溃后旧插件会变孤儿（无 daemon 存活探测）；新 daemon 起来会拉新
//! 插件。两个插件同时推进同一队列 = 同一张卡被重复启动。这把锁把「同时只有
//! 一个推进者」变成不变量；孤儿则靠 daemon 失联即退自行让位。
//!
//! `flock` 而不是 PID 文件：内核在持有进程退出时释放锁，`SIGKILL` 也算。
//! PID 文件做不到——PID 会被回收，一个死插件留下的 PID 会被当成不相干的
//! 进程，锁就永久占死了。

use std::fs::{File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

pub struct InstanceLock {
    /// 关闭这个描述符就是全部释放协议，所以它从不被读——只为把锁按住而保活。
    _file: File,
    path: PathBuf,
}

impl InstanceLock {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// 取 `<state_dir>/plugin.lock` 上的独占锁；已被占用返回 `None`。
///
/// 锁随 fd 关闭由内核释放（含 `SIGKILL`），所以**不在 drop 时 unlink**：删除
/// 文件会让另一个进程在旧 inode 上持锁、在新 inode 上再取到锁，等于没锁。
pub fn acquire(state_dir: &Path) -> Option<InstanceLock> {
    std::fs::create_dir_all(state_dir).ok()?;
    let path = state_dir.join("plugin.lock");
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .ok()?;
    let outcome = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if outcome == 0 {
        Some(InstanceLock { _file: file, path })
    } else {
        None
    }
}

/// 连续失联计数：达到阈值就该退出，一次成功即清零。
///
/// 用「连续」而不是累计，是因为 daemon 短暂卡顿不该把插件赶走；只有一直探不到
/// 才说明它真没了。
#[derive(Debug, Default)]
pub struct Liveness {
    misses: u32,
}

impl Liveness {
    /// 记录一次探测结果，返回是否应退出。
    pub fn observe(&mut self, ok: bool, threshold: u32) -> bool {
        if ok {
            self.misses = 0;
            false
        } else {
            self.misses += 1;
            self.misses >= threshold
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_second_instance_cannot_take_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let first = acquire(dir.path()).expect("first instance takes the lock");
        assert!(
            first.path().ends_with("plugin.lock"),
            "锁文件必须是 <state_dir>/plugin.lock"
        );
        assert!(
            acquire(dir.path()).is_none(),
            "a second instance must be refused"
        );
        drop(first);
        assert!(acquire(dir.path()).is_some(), "released on drop");
    }

    /// 被 `SIGKILL` 的持有者留下的锁必须能被回收。
    ///
    /// 持有者是这个测试二进制自身，带 `PLUGIN_LOCK_HOLD_DIR` 重起，只跑
    /// [`lock_holder_child`]：它取锁然后睡。用 `-9` 杀掉它跳过了任何 unwinding，
    /// 所以唯一能释放锁的就是内核在进程退出时丢掉 `flock`——这正是「孤儿不会
    /// 把推进权占死」所依赖的性质。
    #[test]
    fn a_killed_holder_does_not_deadlock_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let mut holder = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "single_instance::tests::lock_holder_child",
                "--nocapture",
            ])
            .env("PLUGIN_LOCK_HOLD_DIR", dir.path())
            .spawn()
            .expect("re-invoke the test binary as a lock holder");

        // 等子进程真正占住锁。
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut held = false;
        while std::time::Instant::now() < deadline {
            if acquire(dir.path()).is_none() {
                held = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(held, "子进程必须在时限内占住锁");

        let status = std::process::Command::new("kill")
            .args(["-9", &holder.id().to_string()])
            .status()
            .expect("kill -9 the holder");
        assert!(status.success(), "kill -9 必须成功");
        // 收尸：`kill -9` 之后必须 `wait()`，否则子进程成为僵尸。
        let _ = holder.wait();

        // 内核随进程拆除异步释放 flock，所以轮询到能再取为止。
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut reclaimed = false;
        while std::time::Instant::now() < deadline {
            if acquire(dir.path()).is_some() {
                reclaimed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            reclaimed,
            "SIGKILL 之后锁必须可再取，否则孤儿会永久占死推进权"
        );
    }

    /// 仅在以 `PLUGIN_LOCK_HOLD_DIR` 重起时生效的持有者；正常测试运行立即返回。
    #[test]
    fn lock_holder_child() {
        let Ok(dir) = std::env::var("PLUGIN_LOCK_HOLD_DIR") else {
            return;
        };
        let _lock = acquire(Path::new(&dir)).expect("holder must take the lock");
        println!("LOCK-HELD");
        std::thread::sleep(Duration::from_secs(30));
    }

    #[test]
    fn liveness_exits_after_consecutive_misses_only() {
        let mut liveness = Liveness::default();
        assert!(!liveness.observe(true, 3));
        assert!(!liveness.observe(false, 3));
        assert!(!liveness.observe(true, 3), "a success resets the streak");
        assert!(!liveness.observe(false, 3));
        assert!(!liveness.observe(false, 3));
        assert!(
            liveness.observe(false, 3),
            "three consecutive misses -> should exit"
        );
    }
}
