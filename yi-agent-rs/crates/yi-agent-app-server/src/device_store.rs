//! 已配对设备的持久化表:`~/.yi-agent/devices.json`。
//!
//! 与 `workspace_index.rs` 同一套约定:temp 文件 + rename 原子替换,进程内
//! `Mutex` 串行化读-改-写。撤销 = 删记录,因此 token 立即失效。
//!
//! **为什么还要跨进程锁**:这份表天然被多个进程共享(桌面侧车、任何 `--relay`
//! 实例、CLI 的 `pair list`/`pair code`),而且写得很频繁——`authenticate()` 每次
//! 认证都会 `add` 一次,只为刷新 `last_seen_at`。进程内 `Mutex` 挡不住别的进程,
//! 于是两次「读-改-写」会互相覆盖:一方把 `devices.json` 写成自己读到的旧快照,
//! 另一方刚加的设备就丢了。曾出现整张表被写成 `{"devices":[]}` 的事故——桥接
//! 凭据随之消失,桥接连本机环回 ws 被 4401 拒,中继侧永远等不到 `agent connected`。
//!
//! 因此读-改-写在**内核** `flock` 下串行化(与 `yi-agent-store` 的 `InstanceLock`
//! 同一手法):锁由内核按 inode 授予,持锁进程退出(含崩溃)即自动释放,不会像
//! PID 文件那样被复用 PID 骗到。临时文件名也改成 pid+uuid 唯一,避免一个写者
//! `rename` 走共享临时文件后,另一个写者的 `rename` 报 `ENOENT`。

use std::fs::OpenOptions;
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

use crate::protocol::Scope;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub scope: Scope,
    /// token 的哈希,**绝不落明文**。
    pub token_hash: String,
    pub created_at: i64,
    pub last_seen_at: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct DevicesFile {
    #[serde(default)]
    devices: Vec<Device>,
}

pub struct DeviceStore {
    path: PathBuf,
    lock: Mutex<()>,
}

pub fn default_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".yi-agent").join("devices.json")
}

impl DeviceStore {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            lock: Mutex::new(()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 跨进程排他锁的文件路径。锁文件**永不删除**(见下)。
    fn lock_path(&self) -> PathBuf {
        self.path.with_extension("json.lock")
    }

    /// 取内核 `flock` 排他锁,包住一次读-改-写。
    ///
    /// 这里是**阻塞**取锁(非 `LOCK_NB`):临界区只覆盖一次「读文件 + 原子 rename」,
    /// 极短,不值得为争抢引入重试/退避逻辑。锁随 `File` 的 drop 释放;持有者若崩溃,
    /// 内核在描述符关闭时同样释放,不会留下死锁。
    ///
    /// 锁文件与数据文件分开(`devices.json.lock`),且**不删除**:删除会让「已打开
    /// 同一 inode 的竞争者」锁在一个已 unlink 的文件上,而第三个进程新建同名文件,
    /// 于是两个进程都以为自己持锁——正是 `InstanceLock` 注释里记的那类陷阱。
    fn lock_file(&self) -> io::Result<std::fs::File> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(self.lock_path())?;
        loop {
            let outcome = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if outcome == 0 {
                return Ok(file);
            }
            let error = io::Error::last_os_error();
            // 被信号打断不是错误,重试即可;其余(坏描述符、不支持的 fs)要抛出。
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(error);
        }
    }

    /// 进程内 `Mutex` + 跨进程 `flock` 一起拿,覆盖一次读-改-写。
    fn exclusive(&self) -> io::Result<(MutexGuard<'_, ()>, std::fs::File)> {
        let guard = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        let file = self.lock_file()?;
        Ok((guard, file))
    }

    fn read(&self) -> DevicesFile {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write(&self, file: &DevicesFile) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = serde_json::to_string_pretty(file).map_err(io::Error::other)?;
        // 唯一临时名:并发写者不得共用同一个临时路径,否则一个 `rename` 会
        // `ENOENT` 另一个的,且撕裂的读会看成「一条设备都没有」。pid+uuid 足够。
        let tmp = self.path.with_extension(format!(
            "json.{}.{}.tmp",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, &self.path)
    }

    pub fn list(&self) -> Vec<Device> {
        let _guard = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        self.read().devices
    }

    pub fn get(&self, id: &str) -> Option<Device> {
        self.list().into_iter().find(|d| d.id == id)
    }

    pub fn add(&self, device: Device) -> io::Result<()> {
        let (_guard, _lock) = self.exclusive()?;
        let mut file = self.read();
        file.devices.retain(|d| d.id != device.id);
        file.devices.push(device);
        self.write(&file)
    }

    /// 撤销:返回是否真的删掉了一条记录(幂等)。
    pub fn revoke(&self, id: &str) -> io::Result<bool> {
        let (_guard, _lock) = self.exclusive()?;
        let mut file = self.read();
        let before = file.devices.len();
        file.devices.retain(|d| d.id != id);
        let removed = file.devices.len() != before;
        if removed {
            self.write(&file)?;
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (DeviceStore, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        (DeviceStore::new(dir.path().join("devices.json")), dir)
    }

    fn device(id: &str) -> Device {
        Device {
            id: id.to_string(),
            name: "iPhone".into(),
            scope: Scope::Control,
            token_hash: "hash".into(),
            created_at: 1,
            last_seen_at: 1,
        }
    }

    #[test]
    fn add_then_list_round_trips_through_disk() {
        let (store, _dir) = store();
        store.add(device("d1")).unwrap();
        let listed = store.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "d1");
        // 重新构造一次,证明真的落盘了。
        let reopened = DeviceStore::new(store.path().to_path_buf());
        assert_eq!(reopened.list().len(), 1);
    }

    #[test]
    fn revoke_removes_the_device() {
        let (store, _dir) = store();
        store.add(device("d1")).unwrap();
        assert!(store.revoke("d1").unwrap());
        assert!(store.list().is_empty());
        assert!(!store.revoke("d1").unwrap()); // 幂等
    }

    #[test]
    fn a_missing_file_reads_as_empty_not_an_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = DeviceStore::new(dir.path().join("absent.json"));
        assert!(store.list().is_empty());
    }

    /// 并发写:多个**独立实例**(= 多个进程)同时 `add` 同一份表,必须不失败、
    /// 不丢更新。设备表天然跨进程共享,且 `authenticate` 每次认证都 `add` 刷新
    /// `last_seen_at`,故并发是常态。`pairing.rs` 已为同类风险改用唯一临时名;
    /// 本模块曾用固定 `devices.json.tmp`,并发 `rename` 会 `ENOENT` 并互相覆盖。
    #[test]
    fn concurrent_writers_never_lose_a_device_or_fail() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        const WRITERS: usize = 8;
        const ROUNDS: usize = 40;

        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("devices.json");
        let barrier = Arc::new(Barrier::new(WRITERS));

        let handles: Vec<_> = (0..WRITERS)
            .map(|w| {
                let path = path.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    let store = DeviceStore::new(path);
                    barrier.wait();
                    let mut failures = Vec::new();
                    for r in 0..ROUNDS {
                        let id = format!("dev-{w}-{r}");
                        if let Err(e) = store.add(device(&id)) {
                            failures.push(format!("{id}: {e}"));
                        }
                    }
                    failures
                })
            })
            .collect();

        let mut failures = Vec::new();
        for h in handles {
            failures.extend(h.join().unwrap());
        }
        assert!(
            failures.is_empty(),
            "并发 add 全部必须成功;失败 {} 次: {:?}",
            failures.len(),
            &failures[..failures.len().min(5)]
        );

        let listed = DeviceStore::new(path).list();
        assert_eq!(
            listed.len(),
            WRITERS * ROUNDS,
            "并发写后必须一条不少:期望 {},实际 {}",
            WRITERS * ROUNDS,
            listed.len()
        );
    }
}
