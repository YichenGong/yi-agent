//! 生命周期事务日志。
//!
//! # 为什么需要一个持久化的 phase
//!
//! 一次版本切换跨越多步外部动作（排空、备份、迁移、健康检查、提交、投影），
//! 崩溃可能发生在任意两步之间。恢复时**不能**靠猜："进程重启了，大概还在
//! 备份吧"。所以每一步在开始前先把 phase 落盘，恢复读这个字段，按
//! `docs/superpowers/specs/2026-11-02-plugin-distribution-lifecycle.md` §6.1
//! 的表得到唯一的下一步。
//!
//! # 两条不变量
//!
//! 1. **恢复先于准入**：除 `done` 外任何 phase 都不开放准入。本模块把这条
//!    写成 [`LifecyclePhase::admission_open`]，让恢复逻辑没有"顺手开放"的余地。
//! 2. **`projecting` 不可回滚**：此时新激活已提交，回退等于第二次提交，
//!    风险大于修复投影。见 [`LifecyclePhase::rollbackable`]。
//!
//! # 与 JSON schema 的关系
//!
//! 字段名与 `plugins/contracts/lifecycle-v1.schema.json` 逐字一致
//! （`schemaVersion` / `transactionId` / `sharedConfigRevision`）。schema 里
//! `additionalProperties: false`，所以这里也 `deny_unknown_fields`：
//! 多出来的字段说明写方与读方对格式的理解已经不一致，静默忽略会让恢复
//! 读错 phase 之外的东西。

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::PluginError;
use crate::locks;
use crate::types::{PluginId, ReleaseId};

/// 日志格式版本。读时版本不符即拒绝，不猜。
pub const JOURNAL_SCHEMA_VERSION: u32 = 1;

/// 日志短锁等待上限。只包一次读改写。
const JOURNAL_LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// `LifecycleRequest` 只允许这四种明确动作。
///
/// 包下载**不**借用 `Activate` 隐式启用项目：那会让"下载了一个包"变成
/// "授予了权限"。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LifecycleAction {
    Activate,
    DisableProject,
    Uninstall,
    Rollback,
}

impl LifecycleAction {
    pub fn as_str(self) -> &'static str {
        match self {
            LifecycleAction::Activate => "Activate",
            LifecycleAction::DisableProject => "DisableProject",
            LifecycleAction::Uninstall => "Uninstall",
            LifecycleAction::Rollback => "Rollback",
        }
    }
}

/// 冻结的八个持久化阶段，按顺序。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecyclePhase {
    Draining,
    Drained,
    BackingUp,
    Migrating,
    HealthChecking,
    Committing,
    Projecting,
    Done,
}

impl LifecyclePhase {
    /// 按 §6.1 的顺序给出下一个阶段。`done` 没有下一个。
    pub fn next(self) -> Option<LifecyclePhase> {
        Some(match self {
            LifecyclePhase::Draining => LifecyclePhase::Drained,
            LifecyclePhase::Drained => LifecyclePhase::BackingUp,
            LifecyclePhase::BackingUp => LifecyclePhase::Migrating,
            LifecyclePhase::Migrating => LifecyclePhase::HealthChecking,
            LifecyclePhase::HealthChecking => LifecyclePhase::Committing,
            LifecyclePhase::Committing => LifecyclePhase::Projecting,
            LifecyclePhase::Projecting => LifecyclePhase::Done,
            LifecyclePhase::Done => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            LifecyclePhase::Draining => "draining",
            LifecyclePhase::Drained => "drained",
            LifecyclePhase::BackingUp => "backing_up",
            LifecyclePhase::Migrating => "migrating",
            LifecyclePhase::HealthChecking => "health_checking",
            LifecyclePhase::Committing => "committing",
            LifecyclePhase::Projecting => "projecting",
            LifecyclePhase::Done => "done",
        }
    }

    /// 崩溃后读到这个 phase 时应当做什么。
    ///
    /// 这张表就是 §6.1；`every_phase_declares_its_recovery_action` 逐项钉住它，
    /// 免得日后加一个 phase 却忘了写恢复动作——那样恢复会"跳过"它继续往下，
    /// 而下一步依赖的前提根本没建立。
    pub fn recovery_action(self) -> &'static str {
        match self {
            LifecyclePhase::Draining => {
                "re-establish the barrier, then re-read the turns the journal accepted"
            }
            LifecyclePhase::Drained => {
                "confirm every before-barrier turn released its permit, then take the backup"
            }
            LifecyclePhase::BackingUp => {
                "discard the incomplete backup and redo it; the old version is still intact"
            }
            LifecyclePhase::Migrating => {
                "restore the affected projects from backup, then redo the migration idempotently"
            }
            LifecyclePhase::HealthChecking => {
                "if any check is unhealthy, stop the candidate and fall back to the previous activation"
            }
            LifecyclePhase::Committing => {
                "treat the commit as not yet durable: re-read the journal, redo the atomic commit, or roll back"
            }
            LifecyclePhase::Projecting => {
                "repair the project projection from the committed activation, then open admission"
            }
            LifecyclePhase::Done => "open admission",
        }
    }

    /// 是否还能安全退回旧版本。
    ///
    /// `projecting` 之后不能：提交已经发生，回退就是第二次提交。
    pub fn rollbackable(self) -> bool {
        !matches!(self, LifecyclePhase::Projecting | LifecyclePhase::Done)
    }

    /// 准入是否开放。**只有 `done`**。
    pub fn admission_open(self) -> bool {
        matches!(self, LifecyclePhase::Done)
    }

    /// 该阶段是否意味着候选服务可能已经启动。
    ///
    /// 恢复时用它决定"要不要停候选"：`draining` 阶段候选还没起，去停它
    /// 只会让录制到的动作与真实发生的对不上。
    pub fn candidate_may_be_running(self) -> bool {
        matches!(
            self,
            LifecyclePhase::HealthChecking
                | LifecyclePhase::Committing
                | LifecyclePhase::Projecting
                | LifecyclePhase::Done
        )
    }
}

/// 事务目标：要切到的发布身份与激活代次。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalTarget {
    pub release: ReleaseId,
    /// 必须 >= 1：0 不是"没有"，而是无效值。
    pub activation: u64,
}

/// 事务开始前的激活，用于回滚。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviousActivation {
    pub activation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// 屏障还在等什么。列出来是为了让用户看到"哪个项目/回合挡住了更新"，
/// 而不是一个没有解释的超时。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Blocking {
    #[serde(default)]
    pub projects: Vec<String>,
    #[serde(default)]
    pub turns: Vec<String>,
}

/// 日志里的错误，`kind` 是稳定线协议值。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalError {
    pub kind: JournalErrorKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JournalErrorKind {
    Dependency,
    Conflict,
    Draining,
    ResultUnknown,
    RecoveryRequired,
    Io,
    Incompatible,
    Integrity,
}

impl JournalErrorKind {
    pub fn from_error(error: &PluginError) -> JournalErrorKind {
        match error {
            PluginError::Dependency(_) => JournalErrorKind::Dependency,
            PluginError::Conflict(_) => JournalErrorKind::Conflict,
            PluginError::Draining(_) => JournalErrorKind::Draining,
            PluginError::ResultUnknown(_) => JournalErrorKind::ResultUnknown,
            PluginError::RecoveryRequired(_) => JournalErrorKind::RecoveryRequired,
            PluginError::Incompatible(_) => JournalErrorKind::Incompatible,
            PluginError::Integrity(_) => JournalErrorKind::Integrity,
            _ => JournalErrorKind::Io,
        }
    }
}

impl From<&PluginError> for JournalError {
    fn from(error: &PluginError) -> JournalError {
        JournalError {
            kind: JournalErrorKind::from_error(error),
            message: error.to_string(),
        }
    }
}

/// 一次生命周期事务的持久化记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Journal {
    pub schema_version: u32,
    pub transaction_id: String,
    pub plugin: PluginId,
    pub action: LifecycleAction,
    pub phase: LifecyclePhase,
    pub target: JournalTarget,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<PreviousActivation>,
    #[serde(default)]
    pub blocking: Blocking,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_config_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JournalError>,
}

impl Journal {
    /// 新建一份处于 `draining` 的事务。
    pub fn start(
        transaction_id: impl Into<String>,
        plugin: PluginId,
        action: LifecycleAction,
        target: JournalTarget,
        previous: Option<PreviousActivation>,
        shared_config_revision: Option<u64>,
    ) -> Result<Journal, PluginError> {
        let journal = Journal {
            schema_version: JOURNAL_SCHEMA_VERSION,
            transaction_id: transaction_id.into(),
            plugin,
            action,
            phase: LifecyclePhase::Draining,
            target,
            previous,
            blocking: Blocking::default(),
            shared_config_revision,
            error: None,
        };
        journal.validate()?;
        Ok(journal)
    }

    /// 校验所有"能用 JSON 表达但语义非法"的字段。
    ///
    /// serde 只管形状：`activation: 0`、`sha256: "zz"`、`transactionId: "x"`
    /// 都能反序列化成功。这些值一旦进入恢复逻辑，会让"下一步做什么"建立在
    /// 一个不存在的身份上。
    pub fn validate(&self) -> Result<(), PluginError> {
        if self.schema_version != JOURNAL_SCHEMA_VERSION {
            return Err(PluginError::Incompatible(format!(
                "lifecycle journal has schemaVersion {}, this build understands {}",
                self.schema_version, JOURNAL_SCHEMA_VERSION
            )));
        }
        if !valid_transaction_id(&self.transaction_id) {
            return Err(PluginError::InvalidManifest(format!(
                "transaction id {:?} must match ^tx-[a-z0-9-]{{8,63}}$",
                self.transaction_id
            )));
        }
        if self.target.activation == 0 {
            return Err(PluginError::InvalidManifest(
                "target activation must be at least 1; 0 is not \"none\", it is invalid"
                    .to_string(),
            ));
        }
        if let Some(previous) = &self.previous
            && previous.activation == 0
        {
            return Err(PluginError::InvalidManifest(
                "previous activation must be at least 1".to_string(),
            ));
        }
        if self.target.release.plugin != self.plugin {
            return Err(PluginError::InvalidManifest(format!(
                "target release is for plugin `{}`, but the journal is for `{}`",
                self.target.release.plugin.as_str(),
                self.plugin.as_str()
            )));
        }
        validate_release(&self.target.release)
    }
}

fn validate_release(release: &ReleaseId) -> Result<(), PluginError> {
    if !valid_semver(&release.version) {
        return Err(PluginError::InvalidManifest(format!(
            "release version {:?} must be three dot-separated integers",
            release.version
        )));
    }
    if !valid_target(&release.target) {
        return Err(PluginError::InvalidManifest(format!(
            "release target {:?} must be one of darwin/linux/win32 on aarch64/x86_64",
            release.target
        )));
    }
    if !valid_sha256(&release.sha256) {
        return Err(PluginError::InvalidManifest(format!(
            "release digest {:?} must be 64 lowercase hex characters",
            release.sha256
        )));
    }
    Ok(())
}

fn valid_transaction_id(raw: &str) -> bool {
    let Some(rest) = raw.strip_prefix("tx-") else {
        return false;
    };
    (8..=63).contains(&rest.len())
        && rest
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn valid_semver(raw: &str) -> bool {
    let parts: Vec<&str> = raw.split('.').collect();
    parts.len() == 3
        && parts.iter().all(|part| {
            !part.is_empty()
                && (part == &"0" || !part.starts_with('0'))
                && part.chars().all(|c| c.is_ascii_digit())
        })
}

fn valid_target(raw: &str) -> bool {
    let Some((os, arch)) = raw.split_once('-') else {
        return false;
    };
    matches!(os, "darwin" | "linux" | "win32") && matches!(arch, "aarch64" | "x86_64")
}

fn valid_sha256(raw: &str) -> bool {
    raw.len() == 64
        && raw
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

/// 每实例每插件一份日志：`plugins/lifecycle/<plugin>.json`。
///
/// 分开存而不是一份总表，是因为两个插件的事务互不依赖：一个插件的
/// 坏日志不该让另一个插件也无法恢复。
#[derive(Debug, Clone)]
pub struct JournalStore {
    home: PathBuf,
}

impl JournalStore {
    /// 打开并**校验**所有已有日志。坏日志显式报错，不当成"没有事务"。
    ///
    /// 适合"启动时先恢复再放行"的路径：任何一份坏日志都该挡住整个宿主，
    /// 因为那说明上次有人在写到一半时被打断。
    pub fn open(home: impl Into<PathBuf>) -> Result<JournalStore, PluginError> {
        let store = JournalStore { home: home.into() };
        store.load_all()?;
        Ok(store)
    }

    /// 只定位目录，不预读任何日志。
    ///
    /// 给"每次决策单独查一个插件"的调用方用（维护闸门）。两处语义不同：
    /// [`JournalStore::open`] 的失败粒度是**整个实例**，而闸门只需要关心
    /// **当前这个插件**——别的插件日志坏了不该让本插件也启动不了，否则一份
    /// 无关的坏文件就能让所有插件永久停摆，那比它想防的问题更糟。
    /// 损坏仍会经由 [`JournalStore::load`] 报 `RecoveryRequired`。
    pub fn at(home: impl Into<PathBuf>) -> JournalStore {
        JournalStore { home: home.into() }
    }

    fn dir(&self) -> PathBuf {
        self.home.join("plugins").join("lifecycle")
    }

    pub fn path(&self, plugin: &PluginId) -> PathBuf {
        self.dir().join(format!("{}.json", plugin.as_str()))
    }

    /// 事务日志与生命周期锁共用一把长锁。
    pub fn lock_path(&self) -> PathBuf {
        locks::lifecycle_lock(&self.home)
    }

    /// 读一份日志。不存在返回 `None`——这不是错误，只是"没有进行中的事务"。
    pub fn load(&self, plugin: &PluginId) -> Result<Option<Journal>, PluginError> {
        let path = self.path(plugin);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(PluginError::Io(format!("{}: {error}", path.display()))),
        };
        let journal: Journal = serde_json::from_slice(&bytes).map_err(|error| {
            PluginError::RecoveryRequired(format!(
                "lifecycle journal at {} is unreadable ({error}); treating it as \"no transaction\" \
                 would open admission before the interrupted steps are finished",
                path.display()
            ))
        })?;
        journal.validate()?;
        Ok(Some(journal))
    }

    /// 读出目录下所有日志。
    pub fn load_all(&self) -> Result<Vec<Journal>, PluginError> {
        let dir = self.dir();
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(PluginError::Io(format!("{}: {error}", dir.display()))),
        };
        let mut journals = Vec::new();
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            // 只读普通文件：子目录（备份、状态）不是日志。
            if !path.is_file() {
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let bytes = std::fs::read(&path)?;
            let journal: Journal = serde_json::from_slice(&bytes).map_err(|error| {
                PluginError::RecoveryRequired(format!(
                    "lifecycle journal at {} is unreadable ({error})",
                    path.display()
                ))
            })?;
            journal.validate()?;
            journals.push(journal);
        }
        journals.sort_by(|a, b| a.plugin.as_str().cmp(b.plugin.as_str()));
        Ok(journals)
    }

    /// 有未完成的事务吗？
    pub fn unfinished(&self) -> Result<Vec<Journal>, PluginError> {
        Ok(self
            .load_all()?
            .into_iter()
            .filter(|journal| !journal.phase.admission_open())
            .collect())
    }

    /// 原子写入：唯一临时文件 → fsync → rename → fsync 目录。
    ///
    /// 恢复的正确性依赖"读到新 phase 或旧 phase，绝不读到半份 JSON"。
    pub fn write(&self, journal: &Journal) -> Result<(), PluginError> {
        journal.validate()?;
        let _guard = locks::acquire(&self.lock_path(), JOURNAL_LOCK_TIMEOUT)?;
        self.write_locked(journal)
    }

    /// 调用方已持有生命周期锁时用这个，避免同进程重入把自己挡住。
    pub(crate) fn write_locked(&self, journal: &Journal) -> Result<(), PluginError> {
        let path = self.path(&journal.plugin);
        let parent = path.parent().expect("journal path has a parent");
        std::fs::create_dir_all(parent)?;
        let bytes = serde_json::to_vec_pretty(journal)
            .map_err(|e| PluginError::Io(format!("could not serialize the journal: {e}")))?;
        write_atomic(parent, &path, &bytes)
    }

    /// 事务正常收尾后删除日志。
    pub fn remove(&self, plugin: &PluginId) -> Result<(), PluginError> {
        let _guard = locks::acquire(&self.lock_path(), JOURNAL_LOCK_TIMEOUT)?;
        self.remove_locked(plugin)
    }

    pub(crate) fn remove_locked(&self, plugin: &PluginId) -> Result<(), PluginError> {
        let path = self.path(plugin);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(PluginError::Io(format!("{}: {error}", path.display()))),
        }
    }
}

/// 原子替换一个文件。`parent` 必须已经存在。
pub(crate) fn write_atomic(parent: &Path, path: &Path, bytes: &[u8]) -> Result<(), PluginError> {
    use std::io::Write;

    let temp = parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("state"),
        std::process::id(),
        unique_suffix()
    ));
    let result = (|| -> Result<(), PluginError> {
        let mut handle = std::fs::File::create(&temp)?;
        handle.write_all(bytes)?;
        // 内容先落盘，否则 rename 成功但断电后是一个"存在且解析失败"的文件。
        handle.sync_all()?;
        std::fs::rename(&temp, path)?;
        // rename 本身也要落盘，否则崩溃后可能回到旧文件。
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn unique_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos}-{n}")
}
