//! 已配对设备的持久化表:`~/.yi-agent/devices.json`。
//!
//! 与 `workspace_index.rs` 同一套约定:temp 文件 + rename 原子替换,进程内
//! `Mutex` 串行化读-改-写。撤销 = 删记录,因此 token 立即失效。

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

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
        let tmp = self.path.with_extension("json.tmp");
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
        let _guard = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        let mut file = self.read();
        file.devices.retain(|d| d.id != device.id);
        file.devices.push(device);
        self.write(&file)
    }

    /// 撤销:返回是否真的删掉了一条记录(幂等)。
    pub fn revoke(&self, id: &str) -> io::Result<bool> {
        let _guard = self.lock.lock().unwrap_or_else(|p| p.into_inner());
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
}
