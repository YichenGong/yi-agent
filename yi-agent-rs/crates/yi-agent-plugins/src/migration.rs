//! 迁移备份：清单、完整性与幂等重做。
//!
//! # 为什么备份要带清单
//!
//! 备份的用途只有两个：迁移失败后把项目数据退回去，以及恢复后判断"这份
//! 备份是不是完整的"。第二个用途决定了不能只把目录拷一份就算完——半份
//! 备份（`draining`/`backing_up` 期间崩溃）看起来和完整备份一模一样，
//! 用它恢复会把项目数据截断到某个中间状态。
//!
//! 所以清单记录每个文件的大小与摘要，恢复前逐项校验。
//!
//! # 备份里不能有瞬态资源
//!
//! 设计 §9.2 明写：不备份 socket、PID 文件、活锁。带进去会让恢复后出现
//! "幽灵持有者"——一个指向早就不存在的进程的锁文件，而存活判定只认
//! `flock`，于是它永远显示"仍被持有"，屏障永远等不完。

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::PluginError;
use crate::journal::write_atomic;
use crate::locks;

/// 清单格式版本。
pub const BACKUP_SCHEMA_VERSION: u32 = 1;

const BACKUP_LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// 一份被备份的文件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackupEntry {
    /// 相对备份根的路径，用 `/` 分隔。
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

/// 备份清单。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackupManifest {
    pub schema_version: u32,
    pub transaction_id: String,
    /// 备份时的共享配置修订。回退前要重新读一次，前进了就拒绝回退。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_config_revision: Option<u64>,
    pub entries: Vec<BackupEntry>,
    /// 备份是否已经写完。**只有 `true` 的备份可以被用来恢复。**
    ///
    /// 没有这个标记时，一份在中途崩溃留下的清单与完整清单一模一样
    /// （每个已写条目都自洽），恢复会拿它当完整的用。
    pub complete: bool,
}

impl BackupManifest {
    pub fn new(transaction_id: impl Into<String>, shared_config_revision: Option<u64>) -> Self {
        BackupManifest {
            schema_version: BACKUP_SCHEMA_VERSION,
            transaction_id: transaction_id.into(),
            shared_config_revision,
            entries: Vec::new(),
            complete: false,
        }
    }

    pub fn validate(&self) -> Result<(), PluginError> {
        if self.schema_version != BACKUP_SCHEMA_VERSION {
            return Err(PluginError::Incompatible(format!(
                "backup manifest has schemaVersion {}, this build understands {}",
                self.schema_version, BACKUP_SCHEMA_VERSION
            )));
        }
        for entry in &self.entries {
            if !is_safe_relative(&entry.path) {
                return Err(PluginError::Integrity(format!(
                    "backup entry {:?} is not a safe relative path",
                    entry.path
                )));
            }
        }
        Ok(())
    }

    pub fn entry(&self, path: &str) -> Option<&BackupEntry> {
        self.entries.iter().find(|entry| entry.path == path)
    }
}

/// 备份里绝不出现的东西。
///
/// 按文件名与扩展名判断，因为它们在磁盘上的位置随平台与配置变化，
/// 但名字是稳定的。
fn is_transient(name: &str) -> bool {
    let lowered = name.to_ascii_lowercase();
    lowered.ends_with(".sock")
        || lowered.ends_with(".socket")
        || lowered.ends_with(".pid")
        || lowered.ends_with(".lock")
        || lowered == "pid"
        || lowered == "socket"
}

/// 拒绝 `..`、绝对路径、空路径与反斜杠分隔。
fn is_safe_relative(path: &str) -> bool {
    if path.is_empty() || path.starts_with('/') || path.contains('\\') {
        return false;
    }
    path.split('/')
        .all(|part| !part.is_empty() && part != "." && part != "..")
}

/// 备份根：`plugins/backups/<transactionId>/`。
#[derive(Debug, Clone)]
pub struct Backup {
    root: PathBuf,
    transaction_id: String,
}

impl Backup {
    pub fn new(home: &Path, transaction_id: impl Into<String>) -> Backup {
        let transaction_id = transaction_id.into();
        let root = home.join("plugins").join("backups").join(&transaction_id);
        Backup {
            root,
            transaction_id,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn manifest_path(&self) -> PathBuf {
        self.root.join("manifest.json")
    }

    /// 从源目录把文件复制进备份，并写清单。
    ///
    /// `sources` 是 `(相对路径, 源文件)`。调用方已经决定备份什么；
    /// 这里负责：跳过瞬态资源、拒绝不安全路径、复制后算摘要、最后才把
    /// `complete` 置真。
    ///
    /// **顺序很重要**：清单始终标 `complete: false`，直到所有文件都复制
    /// 完并逐项校验通过。中途崩溃留下的清单因此不会被误用。
    pub fn take(
        &self,
        transaction_id: &str,
        shared_config_revision: Option<u64>,
        sources: &[(String, PathBuf)],
    ) -> Result<BackupManifest, PluginError> {
        if transaction_id != self.transaction_id {
            return Err(PluginError::Conflict(format!(
                "backup root is for transaction `{}`, not `{transaction_id}`",
                self.transaction_id
            )));
        }

        // 上一份残缺备份先扔掉：留着会让"这次是重做"与"上次做到一半"
        // 混在一起。
        if self.root.exists() {
            std::fs::remove_dir_all(&self.root)?;
        }
        std::fs::create_dir_all(&self.root)?;

        let mut manifest = BackupManifest::new(transaction_id, shared_config_revision);
        manifest.validate()?;
        self.write_manifest(&manifest)?;

        for (relative, source) in sources {
            if !is_safe_relative(relative) {
                return Err(PluginError::Integrity(format!(
                    "refusing to back up {relative:?}: not a safe relative path"
                )));
            }
            if is_transient(relative) {
                // 不是错误：这些东西本来就不该备份，跳过是对的。
                continue;
            }
            let destination = self.root.join(relative);
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(source, &destination).map_err(|error| {
                PluginError::Io(format!(
                    "could not back up {} to {}: {error}",
                    source.display(),
                    destination.display()
                ))
            })?;
            let bytes = std::fs::read(&destination)?;
            manifest.entries.push(BackupEntry {
                path: relative.clone(),
                size: bytes.len() as u64,
                sha256: sha256_hex(&bytes),
            });
        }

        manifest.entries.sort_by(|a, b| a.path.cmp(&b.path));
        // 先写一份 still-incomplete 的清单（含条目），再置 complete 并重写。
        // 这样读者看到的要么是"没有条目"，要么是"条目齐全但可能不可用"，
        // 永远不会看到"条目齐全且声称完整"的半份备份。
        self.write_manifest(&manifest)?;
        manifest.complete = true;
        self.write_manifest(&manifest)?;
        Ok(manifest)
    }

    /// 读清单并逐项校验完整性。
    pub fn verify(&self) -> Result<BackupManifest, PluginError> {
        let path = self.manifest_path();
        let bytes = std::fs::read(&path).map_err(|error| {
            PluginError::RecoveryRequired(format!(
                "backup manifest at {} is missing ({error}); the transaction wrote a phase \
                 that assumes a backup",
                path.display()
            ))
        })?;
        let manifest: BackupManifest = serde_json::from_slice(&bytes).map_err(|error| {
            PluginError::RecoveryRequired(format!(
                "backup manifest at {} is unreadable ({error})",
                path.display()
            ))
        })?;
        manifest.validate()?;

        if !manifest.complete {
            return Err(PluginError::RecoveryRequired(format!(
                "backup for transaction `{}` is incomplete; restoring from it would truncate \
                 project data to a half-copied state",
                manifest.transaction_id
            )));
        }

        for entry in &manifest.entries {
            let file = self.root.join(&entry.path);
            let bytes = std::fs::read(&file).map_err(|error| {
                PluginError::RecoveryRequired(format!(
                    "backup file {} is missing ({error})",
                    file.display()
                ))
            })?;
            if bytes.len() as u64 != entry.size || sha256_hex(&bytes) != entry.sha256 {
                return Err(PluginError::RecoveryRequired(format!(
                    "backup file {} does not match its manifest (size {} vs claimed {}, \
                     digest mismatch); restoring it would write a corrupted file",
                    file.display(),
                    bytes.len(),
                    entry.size
                )));
            }
        }
        Ok(manifest)
    }

    /// 把备份里的文件写回目标根。
    ///
    /// 幂等：同样的备份恢复两次得到同样的文件。这与"迁移可重做"是一回事——
    /// 崩溃后重跑整步不需要先判断它做到了哪。
    pub fn restore(&self, destination_root: &Path) -> Result<usize, PluginError> {
        let manifest = self.verify()?;
        let _guard = locks::acquire(
            &destination_root
                .parent()
                .unwrap_or(destination_root)
                .join("plugins")
                .join("backup-restore.lock"),
            BACKUP_LOCK_TIMEOUT,
        )?;

        let mut restored = 0;
        for entry in &manifest.entries {
            let source = self.root.join(&entry.path);
            let destination = destination_root.join(&entry.path);
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // 用"写临时文件再 rename"而不是直接 copy：半份恢复文件与
            // 半份备份一样危险，只是危险发生的时间更晚。
            let bytes = std::fs::read(&source)?;
            let parent = destination.parent().expect("destination has a parent");
            write_atomic(parent, &destination, &bytes)?;
            restored += 1;
        }
        Ok(restored)
    }

    fn write_manifest(&self, manifest: &BackupManifest) -> Result<(), PluginError> {
        std::fs::create_dir_all(&self.root)?;
        let bytes = serde_json::to_vec_pretty(manifest).map_err(|e| {
            PluginError::Io(format!("could not serialize the backup manifest: {e}"))
        })?;
        write_atomic(&self.root, &self.manifest_path(), &bytes)
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
