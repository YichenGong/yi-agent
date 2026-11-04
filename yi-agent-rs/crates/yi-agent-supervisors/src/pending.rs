//! 待清理记录：卸载时没能清干净的项目，留在盘上等下一次。
//!
//! # 为什么必须落盘
//!
//! "待清理"如果只在卸载的返回值里，它就是一个**内存中的歉意**：进程一重启
//! 就没了，而留在项目里的 supervisor 清单还在，项目下次可达时照样把程序拉
//! 起来——用户以为已经卸掉了。
//!
//! 设计 §12 要求"保留待清理记录及可见错误"。记录要能被下一次读取看到，所以
//! 只能落盘。
//!
//! # 记录不是授权
//!
//! 一条待清理记录只说明"这里还有东西没删干净"。它不让插件继续运行，也不让
//! 重装自动恢复过去的授权——那是另一套记录（`grants`）的事。
//!
//! # 写入是原子的
//!
//! 与注册表同一套做法：唯一临时文件 → fsync → rename → fsync 目录。读者
//! 要么看到旧内容，要么看到新内容；一个截断的 JSON 会被当成"空表"，而那正好
//! 意味着所有待清理记录被静默丢弃。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use yi_agent_plugins::{PluginError, PluginId};

/// 一份待清理记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PendingCleanupRecord {
    pub plugin: PluginId,
    pub project: String,
    /// 逐条说明还剩什么、为什么没删掉。
    pub remaining: Vec<String>,
    /// 记录时刻（Unix 秒），由调用方注入。
    pub recorded_at: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PendingFile {
    schema_version: u32,
    records: Vec<PendingCleanupRecord>,
}

/// 待清理记录表。句柄不持有锁，每次操作自己取短锁。
#[derive(Debug, Clone)]
pub struct PendingCleanup {
    home: PathBuf,
}

impl PendingCleanup {
    pub fn open(instance_home: impl Into<PathBuf>) -> PendingCleanup {
        PendingCleanup {
            home: instance_home.into(),
        }
    }

    fn path(&self) -> PathBuf {
        self.home.join("plugins").join("pending-cleanup.json")
    }

    fn lock_path(&self) -> PathBuf {
        yi_agent_plugins::locks::registry_lock(&self.home)
    }

    /// 读出全部记录。文件不存在 = 没有待清理项。
    ///
    /// 读不懂就报错，**不**当空表：静默丢弃会让用户永远看不到那些残留，
    /// 而残留恰好是"卸载没完成"的唯一证据。
    pub fn list(&self) -> Result<Vec<PendingCleanupRecord>, PluginError> {
        let path = self.path();
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(PluginError::Io(format!(
                    "could not read {}: {error}",
                    path.display()
                )));
            }
        };
        let file: PendingFile = serde_json::from_slice(&bytes).map_err(|error| {
            PluginError::Io(format!(
                "{} is not a pending-cleanup table: {error}",
                path.display()
            ))
        })?;
        Ok(file.records)
    }

    /// 记下一个项目的残留。同一 `(plugin, project)` 覆盖旧记录。
    pub fn record(&self, record: PendingCleanupRecord) -> Result<(), PluginError> {
        self.write(|records| {
            records.retain(|existing| {
                !(existing.plugin == record.plugin && existing.project == record.project)
            });
            records.push(record);
            Ok(())
        })
    }

    /// 清掉某个项目（或某个插件的全部）记录。清理成功后调用。
    pub fn clear(&self, plugin: &PluginId, project: Option<&str>) -> Result<(), PluginError> {
        self.write(|records| {
            records.retain(|existing| {
                existing.plugin != *plugin
                    || project.is_some_and(|project| existing.project != project)
            });
            Ok(())
        })
    }

    fn write<R>(&self, mutate: R) -> Result<(), PluginError>
    where
        R: FnOnce(&mut Vec<PendingCleanupRecord>) -> Result<(), PluginError>,
    {
        let _guard =
            yi_agent_plugins::locks::acquire(&self.lock_path(), std::time::Duration::from_secs(5))?;
        let mut records = self.list()?;
        mutate(&mut records)?;
        let path = self.path();
        let parent = path.parent().ok_or_else(|| {
            PluginError::Io(format!("{} has no parent directory", path.display()))
        })?;
        std::fs::create_dir_all(parent)?;
        let file = PendingFile {
            schema_version: 1,
            records,
        };
        let bytes = serde_json::to_vec_pretty(&file).map_err(|error| {
            PluginError::Io(format!("could not encode pending cleanup: {error}"))
        })?;
        write_atomic(parent, &path, &bytes)
    }
}

/// 同目录 `tmp` + `rename` 的原子写。
///
/// 与注册表同一套理由：只做 `write` 会留下截断的 JSON，而读者把截断当"空表"
/// 就丢掉了全部待清理记录。
fn write_atomic(parent: &Path, path: &Path, bytes: &[u8]) -> Result<(), PluginError> {
    use std::io::Write;

    let temp = parent.join(format!(
        ".pending.{}.{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let result = (|| -> Result<(), PluginError> {
        let mut handle = std::fs::File::create(&temp)?;
        handle.write_all(bytes)?;
        handle.sync_all()?;
        std::fs::rename(&temp, path)?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plugin() -> PluginId {
        PluginId::parse("demo").unwrap()
    }

    #[test]
    fn a_missing_file_is_an_empty_table_not_an_error() {
        let home = tempfile::tempdir().unwrap();
        assert!(PendingCleanup::open(home.path()).list().unwrap().is_empty());
    }

    #[test]
    fn recording_the_same_project_twice_keeps_one_entry() {
        let home = tempfile::tempdir().unwrap();
        let table = PendingCleanup::open(home.path());
        for remaining in [vec!["first".to_string()], vec!["second".to_string()]] {
            table
                .record(PendingCleanupRecord {
                    plugin: plugin(),
                    project: "/p".to_string(),
                    remaining,
                    recorded_at: 1,
                })
                .unwrap();
        }
        let records = table.list().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].remaining, vec!["second".to_string()]);
    }

    #[test]
    fn clearing_one_project_leaves_the_others() {
        let home = tempfile::tempdir().unwrap();
        let table = PendingCleanup::open(home.path());
        for project in ["/a", "/b"] {
            table
                .record(PendingCleanupRecord {
                    plugin: plugin(),
                    project: project.to_string(),
                    remaining: vec!["manifest".to_string()],
                    recorded_at: 1,
                })
                .unwrap();
        }
        table.clear(&plugin(), Some("/a")).unwrap();
        let records = table.list().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].project, "/b");
    }

    /// 坏文件必须报错，不能被当成空表——那正是"静默丢掉全部残留"的样子。
    #[test]
    fn a_corrupt_table_is_reported_not_ignored() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("plugins");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("pending-cleanup.json"), b"{ not json").unwrap();
        assert!(PendingCleanup::open(home.path()).list().is_err());
    }
}
