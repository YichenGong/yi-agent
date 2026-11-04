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
    /// 没清干净的残留，按项目列出。
    ///
    /// 有内容就**不是**一次完全卸载：可能是项目目录此刻不可达（U 盘拔了、
    /// 网络盘断了），也可能是删某个文件失败。设计 §12 要求这时保留「待清理」
    /// 记录并给出可见错误，而不是宣称卸载成功——否则残留的 supervisor 清单
    /// 会在项目重新可达后把程序再拉起来，而用户以为自己已经卸掉了。
    #[serde(default)]
    pub pending: Vec<PendingCleanup>,
}

/// 一个没能清干净的项目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PendingCleanup {
    pub project: String,
    /// 逐条说明还剩什么、为什么没删掉。
    pub remaining: Vec<String>,
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
    uninstall_scoped(instance_home, plugin, remove_data, None)
}

/// 同 [`uninstall`]，但可以限定"只清这些项目的数据"。
///
/// `remove_data` + `Some(projects)` 才是 `clearData` 的正确形状：用户点名了
/// 哪几个项目，就只删哪几个。把所有授权项目一起删掉，再拿名单去过滤**输出**，
/// 是一句"看起来只删了点名的"——盘上的数据已经没了。
pub fn uninstall_scoped(
    instance_home: &Path,
    plugin: &PluginId,
    remove_data: bool,
    scope: Option<&[String]>,
) -> Result<UninstallOutcome, PluginError> {
    // 生命周期锁：卸载与"另一个实例正在激活/回滚"必须互斥。否则一个刚被
    // 卸载的版本可能在同一时刻被写回激活记录，留下指向不存在目录的激活。
    let _lifecycle = locks::acquire(
        &instance_home.join("plugins").join("lifecycle.lock"),
        Duration::from_secs(30),
    )?;
    let pending_table = crate::pending::PendingCleanup::open(instance_home);

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

    // 数据目录：只有显式要求才删，且只删被点名的项目。
    if remove_data {
        let targets: Vec<&String> = match scope {
            Some(named) => {
                // 点名了但不在授权项目里：直接拒绝。静默忽略会让用户以为删了。
                let known: BTreeSet<&str> = projects.iter().map(String::as_str).collect();
                let unknown: Vec<&str> = named
                    .iter()
                    .map(String::as_str)
                    .filter(|name| !known.contains(name))
                    .collect();
                if !unknown.is_empty() {
                    return Err(PluginError::NotFound(format!(
                        "these projects are not authorised for `{}`: {}",
                        plugin.as_str(),
                        unknown.join(", ")
                    )));
                }
                named.iter().collect()
            }
            None => projects.iter().collect(),
        };
        for project in targets {
            if let Some(dir) = plugin_data_dir(plugin, project)
                && dir.exists()
            {
                std::fs::remove_dir_all(&dir).map_err(|error| {
                    PluginError::Io(format!("could not delete {}: {error}", dir.display()))
                })?;
            }
        }
    }

    // 项目级宿主入口：supervisor 清单、开关、日志。
    //
    // 删清单就是「让 supervisor 不再拉起它」的那一下，所以这一步必须在最后
    // ——它之前的任何失败都还允许项目继续使用插件。漏掉这一步的后果不是
    // "留了个文件"，而是**卸载之后程序仍会被重新启动**。
    //
    // 项目不可达时不能当成功：这里把每个项目的结果收集起来，有残留就返回
    // `pending`，让上层显示"待清理"而不是"已卸载"。
    let mut pending = Vec::new();
    for project in &projects {
        let root = Path::new(project);
        if !root.is_dir() {
            pending.push(PendingCleanup {
                project: project.clone(),
                remaining: vec![
                    "the project directory is not reachable, so its supervisor manifest and \
                     switch are still on disk; the plugin would start again if it comes back"
                        .to_string(),
                ],
            });
            continue;
        }
        // 开关键由插件清单声明。读不到就只删清单——不要猜一个键名去删别人的偏好。
        let switch_key = plugin_switch_key(instance_home, plugin, &record);
        let remaining = match switch_key {
            Some(key) => crate::install::uninstall_manifest_for(root, plugin.as_str(), &key),
            None => {
                let mut failures = Vec::new();
                if let Err(error) = crate::install::remove_manifest(root, plugin.as_str()) {
                    failures.push(format!("could not remove the supervisor manifest: {error}"));
                }
                failures
            }
        };
        if !remaining.is_empty() {
            // 落盘：只在返回值里说"还有残留"等于内存中的歉意，进程一重启就
            // 没了，而项目里的清单还在。记录留着，用户下次还能看到它。
            pending_table.record(crate::pending::PendingCleanupRecord {
                plugin: plugin.clone(),
                project: project.clone(),
                remaining: remaining.clone(),
                recorded_at: unix_now(),
            })?;
            pending.push(PendingCleanup {
                project: project.clone(),
                remaining,
            });
        } else {
            // 这个项目清干净了：旧记录（如果有）该消失。
            pending_table.clear(plugin, Some(project))?;
        }
    }

    let versions_removed = record.installed.len();
    registry.remove_plugin(plugin)?;

    Ok(UninstallOutcome {
        plugin: plugin.as_str().to_string(),
        versions_removed,
        grants_revoked: revoked,
        retained,
        pending,
    })
}

/// 从**已安装且已验签**的那份清单里读 `switch_key`。
///
/// 不猜：一个猜出来的键名会被写进共享的 `preferences.json`，删掉的可能是
/// 另一个功能的开关。读不到就返回 `None`，调用方只删清单。
fn plugin_switch_key(
    instance_home: &Path,
    plugin: &PluginId,
    record: &yi_agent_plugins::PluginRecord,
) -> Option<String> {
    let version = record.selected.clone()?;
    let installed = record
        .installed
        .iter()
        .find(|installed| installed.version == version)?;
    let path = instance_home
        .join(&installed.directory)
        .join(crate::managed::MANAGED_MANIFEST);
    let bytes = std::fs::read(&path).ok()?;
    let manifest = yi_agent_plugins::Manifest::parse(&bytes).ok()?;
    // 目录里那份清单必须确实是这个插件的：装错目录、或注册表与磁盘不同步时，
    // 拿别人的 switch_key 去删共享偏好会关掉另一个功能。
    if &manifest.plugin != plugin {
        return None;
    }
    manifest.data.legacy.and_then(|legacy| legacy.switch_key)
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

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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
