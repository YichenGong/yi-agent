//! 停用、卸载、清理与保留数据（设计 §12，计划 T24 的契约面）。
//!
//! # 为什么这一层在 supervisors 而不是 plugins
//!
//! 卸载要同时读写**注册表**（`plugins` 拥有）和**授权表**（本 crate 的
//! `managed::grants` 拥有）。`plugins` 刻意不依赖本 crate，所以把两者编排在
//! 一起的代码只能放在这一侧。这不是"分层泄漏"：它只把两份已经存在的权威
//! 记录读到一处做一次一致的决定，不新造状态。
//!
//! # 三条不可退让的规则
//!
//! 1. **卸载默认保留用户数据。** 用户的数据是用户自己攒的；卸载清理的是
//!    "这个程序"——版本目录、注册记录、授权。删数据只发生在显式的
//!    `remove_data = true`，那是一句独立于卸载的话。
//! 2. **只清引用安全的产物。** [`gc`] 不猜"看起来没人用"。逐个版本问三件事：
//!    是不是当前激活版本？还有没有项目授权引着？是不是还有备份/事务在读？
//!    任一条读不出来都当"仍被引用"——多留一份磁盘，远比删掉一份正在被恢复
//!    事务读的备份安全。
//! 3. **保留数据不是运行授权。** 结果里的 `retained` 只用来说明"删了程序之后
//!    还剩什么"。它不让插件继续运行，也不让重装自动恢复过去的授权。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use yi_agent_plugins::{Backup, Lifecycle, PluginError, PluginId, ProjectId, Registry, locks};

use crate::managed::grants;

/// 一次卸载的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UninstallOutcome {
    pub plugin: String,
    /// 从注册表摘掉的已安装版本数。
    pub versions_removed: usize,
    /// 一并撤销的项目授权数。
    pub grants_revoked: usize,
    /// **保留**的数据，按项目列出。空表示没有需要保留的。
    pub retained: Vec<RetainedData>,
}

/// 卸载后仍然留在磁盘上的用户数据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RetainedData {
    pub project: String,
    /// 数据目录（`None` = 该项目路径无法解析或目录不存在）。
    pub path: Option<String>,
    /// 为什么留着它。给用户看的那一句话。
    pub why: String,
}

/// 一次 [`gc`] 的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GcOutcome {
    /// 删掉的版本目录（相对实例根）。
    pub removed: Vec<String>,
    /// 想删但判定仍被引用的，带原因。
    pub kept: Vec<KeptArtifact>,
}

/// 一个被 [`gc`] 判定为"仍被引用"的产物。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KeptArtifact {
    pub path: String,
    pub why: String,
}

/// 卸载一个插件。`remove_data` 必须由调用方显式传 `true`（对应 `clearData`）。
pub fn uninstall(
    instance_home: &Path,
    plugin: &PluginId,
    remove_data: bool,
) -> Result<UninstallOutcome, PluginError> {
    // 生命周期锁：卸载与"另一个实例正在激活/回滚"必须互斥。否则一个刚被
    // 卸载的版本可能在同一时刻被写回激活记录，留下指向不存在目录的激活。
    let _lifecycle = locks::acquire(
        &instance_home.join("plugins").join("lifecycle.lock"),
        Duration::from_secs(30),
    )?;

    let registry = Registry::open(instance_home)?;
    let record = registry
        .list()?
        .into_iter()
        .find(|record| &record.plugin == plugin)
        .ok_or_else(|| {
            PluginError::NotFound(format!(
                "plugin `{}` is not installed in this instance",
                plugin.as_str()
            ))
        })?;

    // 先读授权：`retained` 要按项目列出来，撤销之后就查不到这些项目了。
    let table = grants::load(instance_home)?;
    let projects: Vec<String> = table
        .all()
        .iter()
        .filter(|grant| grant.plugin == plugin.as_str())
        .map(|grant| grant.project.as_str().to_string())
        .collect();

    let mut retained = Vec::new();
    if !remove_data {
        for project in &projects {
            let data = plugin_data_dir(plugin, project);
            retained.push(RetainedData {
                project: project.clone(),
                path: data
                    .as_ref()
                    .filter(|path| path.exists())
                    .map(|path| path.display().to_string()),
                why: "plugin data is kept by default: uninstalling a program is not a reason \
                      to delete what the user made with it"
                    .to_string(),
            });
        }
    }

    // 撤销授权。失败就整体失败并保留注册记录——"程序没了但授权还在"比
    // "还留着半条记录"更糟。
    let mut revoked = 0;
    for project in &projects {
        let project_id = ProjectId::parse(project)?;
        grants::update(instance_home, |table| {
            table.revoke(plugin.as_str(), &project_id)
        })?;
        revoked += 1;
    }

    // 数据目录：只有显式要求才删。
    if remove_data {
        for project in &projects {
            if let Some(dir) = plugin_data_dir(plugin, project) {
                if dir.exists() {
                    std::fs::remove_dir_all(&dir).map_err(|error| {
                        PluginError::Io(format!("could not delete {}: {error}", dir.display()))
                    })?;
                }
            }
        }
    }

    let versions_removed = record.installed.len();
    registry.remove_plugin(plugin)?;

    Ok(UninstallOutcome {
        plugin: plugin.as_str().to_string(),
        versions_removed,
        grants_revoked: revoked,
        retained,
    })
}

/// 只清理引用安全的受管版本目录。
///
/// 判定顺序是"能不能确定没人要用"，不是"看起来像不像垃圾"。
pub fn gc(instance_home: &Path) -> Result<GcOutcome, PluginError> {
    let _lifecycle = locks::acquire(
        &instance_home.join("plugins").join("lifecycle.lock"),
        Duration::from_secs(30),
    )?;

    let registry = Registry::open(instance_home)?;
    let records = registry.list()?;
    let table = grants::load(instance_home)?;
    let lifecycle = Lifecycle::open(instance_home)?;

    // 仍被授权的 (plugin, version)：这些目录不能删，哪怕没有激活。
    let mut pinned: BTreeSet<(String, String)> = BTreeSet::new();
    for grant in table.all() {
        pinned.insert((grant.plugin.clone(), grant.release.version.clone()));
    }

    // 尚未完成的备份事务正在读它们涉及的东西。读不出清单就当它引用了东西：
    // 一个损坏的备份目录仍可能是某次恢复的唯一线索。
    let mut guarded: BTreeSet<String> = BTreeSet::new();
    let backups = instance_home.join("plugins").join("backups");
    if let Ok(entries) = std::fs::read_dir(&backups) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            guarded.insert(name.clone());
            if let Ok(manifest) = Backup::new(instance_home, &name).verify() {
                for backup_entry in &manifest.entries {
                    guarded.insert(backup_entry.path.clone());
                }
            }
        }
    }

    let mut outcome = GcOutcome {
        removed: Vec::new(),
        kept: Vec::new(),
    };

    for record in &records {
        let active = lifecycle
            .committed(&record.plugin)?
            .map(|committed| committed.release.version);
        for version in &record.installed {
            let key = (record.plugin.as_str().to_string(), version.version.clone());
            if Some(&version.version) == active.as_ref() {
                outcome.kept.push(KeptArtifact {
                    path: version.directory.clone(),
                    why: "this is the committed activation".to_string(),
                });
                continue;
            }
            if pinned.contains(&key) {
                outcome.kept.push(KeptArtifact {
                    path: version.directory.clone(),
                    why: "a project grant still names this version".to_string(),
                });
                continue;
            }
            if guarded.contains(&version.directory) {
                outcome.kept.push(KeptArtifact {
                    path: version.directory.clone(),
                    why: "an unfinished transaction still has a backup of it".to_string(),
                });
                continue;
            }

            let directory = instance_home.join(&version.directory);
            if directory.exists() {
                std::fs::remove_dir_all(&directory).map_err(|error| {
                    PluginError::Io(format!("could not delete {}: {error}", directory.display()))
                })?;
            }
            outcome.removed.push(version.directory.clone());
        }
    }

    Ok(outcome)
}

/// 一个插件在某项目下的数据目录。
///
/// 首版约定在项目内的 `.yi-agent/plugins/<plugin>`：它在**项目**目录里，
/// 卸载宿主级程序不该顺手删它。
fn plugin_data_dir(plugin: &PluginId, project: &str) -> Option<PathBuf> {
    let base = PathBuf::from(project);
    if !base.is_absolute() {
        // 相对路径不是规范项目路径；猜一个"应该在哪"只会删错地方。
        return None;
    }
    Some(base.join(".yi-agent").join("plugins").join(plugin.as_str()))
}

#[cfg(test)]
mod tests;
