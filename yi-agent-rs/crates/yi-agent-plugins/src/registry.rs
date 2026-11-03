//! 实例注册表：记录每个实例装了哪些插件、哪个版本被选中。
//!
//! # 为什么是"每实例一份 JSON + flock"而不是数据库
//!
//! 这个 crate 刻意不依赖 sqlite / app-server / Tauri（T06 的边界）。注册表
//! 的数据量是「几十个插件 × 几个版本」，一份带 `schemaVersion` 的 JSON 就够，
//! 而跨进程一致性由 `locks.rs` 的 `flock` 提供。
//!
//! # 读者永远看不到半个文件
//!
//! 写入走「唯一临时文件 → fsync → rename → fsync 目录」。`rename` 在同一
//! 文件系统上是原子的，所以读者要么看到旧内容，要么看到新内容。只做
//! `write` 会留下截断的 JSON，而读者把截断当"空注册表"就丢掉了全部记录。
//!
//! # 不创建运行授权
//!
//! 安装只设"选中版本"。grant（项目授权）与启动记录属于 T10/T11：设计 §6
//! 明确「安装不产生运行授权」。在这里顺手建一个 grant，等于把"装了"
//! 等同于"用户已同意让它在某个项目里运行"。

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::PluginError;
use crate::locks;
use crate::types::PluginId;

/// 注册表文件的格式版本。读取时版本不符即拒绝，而不是猜。
pub const REGISTRY_SCHEMA_VERSION: u32 = 1;

/// 短锁等待上限。只包一次读改写，正常应毫秒级。
const REGISTRY_LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// 一个已安装版本。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledVersion {
    pub version: String,
    pub sha256: String,
    /// 包内相对路径的安装目录，相对实例根。
    pub directory: String,
    /// 安装时刻（Unix 秒），由调用方注入，registry 不读系统时钟。
    pub installed_at: u64,
}

/// 一个插件的记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginRecord {
    pub plugin: PluginId,
    /// 已安装的版本，按 `version` 字典序保存。
    pub installed: Vec<InstalledVersion>,
    /// 选中（激活）的版本。`None` 表示装了但没有任何版本被选中。
    pub selected: Option<String>,
    /// 项目授权摘要。**T09 不产生它**，字段存在是为了让读者能读到 T10
    /// 之后写下的数据，而不是在 schema 里再加一版。
    #[serde(default)]
    pub grants: Vec<GrantSummary>,
}

/// 项目授权的只读摘要。键只保留安全字符，注释不参与标识。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantSummary {
    pub project: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RegistryFile {
    #[serde(rename = "schemaVersion")]
    schema_version: u32,
    plugins: Vec<PluginRecord>,
}

impl Default for RegistryFile {
    fn default() -> Self {
        RegistryFile {
            schema_version: REGISTRY_SCHEMA_VERSION,
            plugins: Vec::new(),
        }
    }
}

/// 实例注册表句柄。
///
/// 不持有锁：每次写操作自己取短锁。长期持有会让「打开」变成独占，
/// 而读者不该被一个闲置的句柄挡住。
#[derive(Debug, Clone)]
pub struct Registry {
    home: PathBuf,
}

impl Registry {
    /// 打开实例根下的注册表。不创建文件——读一个不存在的注册表返回空表。
    pub fn open(home: impl Into<PathBuf>) -> Result<Registry, PluginError> {
        let home = home.into();
        let registry = Registry { home };
        // 坏数据必须显式报错，不能当空表：把损坏当空表会让下一次写
        // 覆盖掉用户全部安装记录。
        registry.read()?;
        Ok(registry)
    }

    fn path(&self) -> PathBuf {
        self.home.join("plugins").join("registry.json")
    }

    fn lock_path(&self) -> PathBuf {
        locks::registry_lock(&self.home)
    }

    /// 列出所有插件记录。读者不加锁：`rename` 保证看到的是完整文件。
    pub fn list(&self) -> Result<Vec<PluginRecord>, PluginError> {
        Ok(self.read()?.plugins)
    }

    /// 记录一次安装。
    ///
    /// - 同 `(plugin, version, sha256)`：幂等，不改动任何东西，返回现有记录。
    /// - 同 `(plugin, version)` 不同 `sha256`：`Conflict`，绝不覆盖。
    /// - 首次安装：设为选中版本，**不创建 grant**。
    pub fn record_install(
        &self,
        plugin: &PluginId,
        version: &str,
        sha256: &str,
        directory: &str,
        installed_at: u64,
    ) -> Result<PluginRecord, PluginError> {
        // 短锁只包住这一次读改写。任何 IO 等待都必须在这之前完成。
        let _guard = locks::acquire(&self.lock_path(), REGISTRY_LOCK_TIMEOUT)?;
        let mut file = self.read()?;

        let entry = match file.plugins.iter_mut().find(|r| &r.plugin == plugin) {
            Some(existing) => existing,
            None => {
                file.plugins.push(PluginRecord {
                    plugin: plugin.clone(),
                    installed: Vec::new(),
                    selected: None,
                    grants: Vec::new(),
                });
                file.plugins.last_mut().expect("just pushed a record")
            }
        };

        if let Some(existing) = entry.installed.iter().find(|v| v.version == version) {
            if existing.sha256 != sha256 {
                return Err(PluginError::Conflict(format!(
                    "plugin `{}` version `{version}` is already installed with a different \
                     digest ({} vs {sha256}); refusing to overwrite",
                    plugin.as_str(),
                    existing.sha256
                )));
            }
            // 幂等：摘要相同就是同一次安装。不重写、不改选中版本——
            // 调用方可能只是想确认它装过了。
            return Ok(entry.clone());
        }

        entry.installed.push(InstalledVersion {
            version: version.to_string(),
            sha256: sha256.to_string(),
            directory: directory.to_string(),
            installed_at,
        });
        entry.installed.sort_by(|a, b| a.version.cmp(&b.version));
        // 首次安装才设选中版本。已有选中版本时保持不动：升级不该在
        // "装好了"这一刻就悄悄切换用户正在用的版本。
        if entry.selected.is_none() {
            entry.selected = Some(version.to_string());
        }

        let record = entry.clone();
        file.plugins
            .sort_by(|a, b| a.plugin.as_str().cmp(b.plugin.as_str()));
        self.write(&file)?;
        Ok(record)
    }

    /// 设置选中（激活）版本。只允许切到已安装的版本。
    pub fn select_version(
        &self,
        plugin: &PluginId,
        version: &str,
    ) -> Result<PluginRecord, PluginError> {
        let _guard = locks::acquire(&self.lock_path(), REGISTRY_LOCK_TIMEOUT)?;
        let mut file = self.read()?;
        let entry = file
            .plugins
            .iter_mut()
            .find(|r| &r.plugin == plugin)
            .ok_or_else(|| {
                PluginError::NotFound(format!("plugin `{}` is not registered", plugin.as_str()))
            })?;
        if !entry.installed.iter().any(|v| v.version == version) {
            return Err(PluginError::NotFound(format!(
                "plugin `{}` has no installed version `{version}`",
                plugin.as_str()
            )));
        }
        entry.selected = Some(version.to_string());
        let record = entry.clone();
        self.write(&file)?;
        Ok(record)
    }

    /// 删除一个插件的全部注册记录。
    ///
    /// 只动"装了什么"这份账。发布目录由 [`crate::uninstall::gc`] 按引用
    /// 决定去留，用户数据默认保留——卸载不该顺手删掉用户攒下的东西。
    ///
    /// 插件本来就不在注册表里时报 `NotFound`：静默成功会让调用方以为
    /// "删掉了"，而实际它从没被记录过，下一次安装会撞上遗留的版本目录。
    pub fn remove_plugin(&self, plugin: &PluginId) -> Result<PluginRecord, PluginError> {
        let _guard = locks::acquire(&self.lock_path(), REGISTRY_LOCK_TIMEOUT)?;
        let mut file = self.read()?;
        let index = file
            .plugins
            .iter()
            .position(|record| &record.plugin == plugin)
            .ok_or_else(|| {
                PluginError::NotFound(format!(
                    "plugin `{}` is not registered in this instance",
                    plugin.as_str()
                ))
            })?;
        let removed = file.plugins.remove(index);
        self.write(&file)?;
        Ok(removed)
    }

    fn read(&self) -> Result<RegistryFile, PluginError> {
        let path = self.path();
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(RegistryFile::default());
            }
            Err(error) => return Err(PluginError::Io(format!("{}: {error}", path.display()))),
        };
        let file: RegistryFile = serde_json::from_slice(&bytes).map_err(|error| {
            PluginError::RecoveryRequired(format!(
                "registry at {} is unreadable ({error}); refusing to treat it as empty, \
                 because the next write would erase every recorded installation",
                path.display()
            ))
        })?;
        if file.schema_version != REGISTRY_SCHEMA_VERSION {
            return Err(PluginError::Incompatible(format!(
                "registry at {} has schemaVersion {}, this build understands {}",
                path.display(),
                file.schema_version,
                REGISTRY_SCHEMA_VERSION
            )));
        }
        Ok(file)
    }

    fn write(&self, file: &RegistryFile) -> Result<(), PluginError> {
        let path = self.path();
        let parent = path.parent().expect("registry path always has a parent");
        std::fs::create_dir_all(parent)?;

        let bytes = serde_json::to_vec_pretty(file)
            .map_err(|e| PluginError::Io(format!("could not serialize the registry: {e}")))?;

        // 唯一临时文件：固定名字会让两个进程互相截断对方的半成品。
        let temp = parent.join(format!(
            ".registry.{}.{}.tmp",
            std::process::id(),
            unique_suffix()
        ));

        let write_result = (|| -> Result<(), PluginError> {
            use std::io::Write;
            let mut handle = std::fs::File::create(&temp)?;
            handle.write_all(&bytes)?;
            handle.write_all(b"\n")?;
            // fsync 文件：否则 rename 成功但内容仍在页缓存里，断电后
            // 得到的是一个"存在且完整解析失败"的文件。
            handle.sync_all()?;
            std::fs::rename(&temp, &path)?;
            // fsync 目录：否则 rename 本身可能还没落盘。
            fsync_dir(parent)?;
            Ok(())
        })();

        if write_result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        write_result
    }
}

fn fsync_dir(dir: &Path) -> Result<(), PluginError> {
    let handle = std::fs::File::open(dir)?;
    handle.sync_all()?;
    Ok(())
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
