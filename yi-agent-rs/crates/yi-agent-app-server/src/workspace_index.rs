//! 全局「最近目录」索引:`~/.yi-agent/workspaces.json`。
//!
//! 与 thread 数据分离——移除目录只动本文件,不碰 `<dir>/.yi-agent/`。
//! 读-改-写用进程内 `Mutex` 串行化,写入用 temp 文件 + rename 原子替换。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
struct WorkspacesFile {
    #[serde(default)]
    dirs: Vec<String>,
}

pub struct WorkspaceIndex {
    path: PathBuf,
    lock: Mutex<()>,
}

/// 全局索引默认路径:`$HOME/.yi-agent/workspaces.json`。
pub fn default_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".yi-agent").join("workspaces.json")
}

impl WorkspaceIndex {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            lock: Mutex::new(()),
        }
    }

    /// 当前目录列表,最近的在前。
    pub fn list(&self) -> Vec<String> {
        self.read().dirs
    }

    /// 加入并置顶(去重)。path 应为已 canonicalize 的绝对路径。
    pub fn add(&self, path: &Path) -> std::io::Result<()> {
        let entry = path.to_string_lossy().to_string();
        let _guard = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        let mut file = self.read();
        file.dirs.retain(|d| d != &entry);
        file.dirs.insert(0, entry);
        self.write(&file)
    }

    /// 从索引移除;不存在则幂等成功。
    pub fn remove(&self, path: &Path) -> std::io::Result<()> {
        let entry = path.to_string_lossy().to_string();
        let _guard = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        let mut file = self.read();
        file.dirs.retain(|d| d != &entry);
        self.write(&file)
    }

    fn read(&self) -> WorkspacesFile {
        let Ok(raw) = std::fs::read_to_string(&self.path) else {
            return WorkspacesFile::default();
        };
        serde_json::from_str(&raw).unwrap_or_else(|e| {
            eprintln!("[app-server] ignoring corrupt workspaces index: {e}");
            WorkspacesFile::default()
        })
    }

    fn write(&self, file: &WorkspacesFile) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(file)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let tmp = self
            .path
            .with_extension(format!("tmp.{}", std::process::id()));
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn index() -> (TempDir, WorkspaceIndex) {
        let dir = TempDir::new().unwrap();
        let idx = WorkspaceIndex::new(dir.path().join("workspaces.json"));
        (dir, idx)
    }

    #[test]
    fn add_then_list_round_trips() {
        let (_d, idx) = index();
        idx.add(Path::new("/tmp/a")).unwrap();
        idx.add(Path::new("/tmp/b")).unwrap();
        assert_eq!(idx.list(), vec!["/tmp/b".to_string(), "/tmp/a".to_string()]);
    }

    #[test]
    fn add_dedupes_and_moves_to_front() {
        let (_d, idx) = index();
        idx.add(Path::new("/tmp/a")).unwrap();
        idx.add(Path::new("/tmp/b")).unwrap();
        idx.add(Path::new("/tmp/a")).unwrap();
        assert_eq!(idx.list(), vec!["/tmp/a".to_string(), "/tmp/b".to_string()]);
    }

    #[test]
    fn remove_deletes_entry() {
        let (_d, idx) = index();
        idx.add(Path::new("/tmp/a")).unwrap();
        idx.add(Path::new("/tmp/b")).unwrap();
        idx.remove(Path::new("/tmp/a")).unwrap();
        assert_eq!(idx.list(), vec!["/tmp/b".to_string()]);
    }

    #[test]
    fn remove_missing_is_ok() {
        let (_d, idx) = index();
        assert!(idx.remove(Path::new("/tmp/nope")).is_ok());
    }

    #[test]
    fn list_missing_file_is_empty() {
        let (_d, idx) = index();
        assert!(idx.list().is_empty());
    }

    #[test]
    fn corrupt_file_is_treated_as_empty() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("workspaces.json");
        std::fs::write(&path, b"{not json").unwrap();
        let idx = WorkspaceIndex::new(path);
        assert!(idx.list().is_empty());
    }
}
