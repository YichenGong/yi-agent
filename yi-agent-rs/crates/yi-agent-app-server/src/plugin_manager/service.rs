//! 受管插件的管理应用服务。
//!
//! # 为什么它在这里，而不在 CLI
//!
//! 同一套管理动作有两个入口：桌面/RPC（`plugins/v1/*`）与 headless CLI
//! （`yi-agent plugins ...`）。如果两边各写一份"装、启用、激活"，它们迟早
//! 对**同一组磁盘记录**给出不同解释——而这两份记录（registry、grants、
//! lifecycle）正是授权与完整性的唯一依据。分叉的那一天，较弱的那份就是
//! 绕过校验的入口。
//!
//! 所以：本模块是**唯一**实现。CLI 与 RPC 都只是它的调用方，各自只负责把
//! 参数变成调用、把结果变成输出格式。这里不认 HTTP、不认 argv、不认 JSON-RPC。
//!
//! # 它刻意不做什么
//!
//! - **不推进生命周期事务。** `activate` 只 `start` 一个事务；drain、迁移、
//!   健康检查、提交都需要 `RuntimeControl`，那是 daemon 的事。本模块拿不到
//!   运行期控制面，于是如实报告"事务已建立、尚未推进"，而不是假装激活完成。
//! - **不复制验证/下载/事务实现。** 全部委托给 `yi_agent_plugins` 与
//!   `yi_agent_supervisors` 里已有的原语。
//! - **不接受任意 URL 或本地路径当作"发布"。** 下载输入的是一条受信目录项，
//!   源由宿主钉死。

use std::path::{Path, PathBuf};

use yi_agent_plugins::project::ProjectResolver;
use yi_agent_plugins::{
    AllowedOrigin, Binding, Catalog, CatalogEntry, Clock, DownloadLimits, Effect, ExtractLimits,
    Fetcher, Lifecycle, LifecycleRequest, Manifest, PluginError, PluginId, ProjectId, Registry,
    ReleaseId, TrustStore,
};
use yi_agent_supervisors::managed::{self, grants};
use yi_agent_supervisors::uninstall as managed_uninstall;

/// 一个已安装版本在实例根下的标准位置。
///
/// 位置由**清单内容**决定，不由调用方给：允许调用方指定"装到哪"就等于允许
/// 它把包解到受管根之外，而那正是 `install_verified` 要防的事。
pub fn release_directory(manifest: &Manifest) -> String {
    format!(
        "plugins/installed/{}/{}/{}",
        manifest.plugin.as_str(),
        manifest.version,
        manifest.target
    )
}

/// 宿主的平台标识（`darwin-aarch64` 这类）。
pub fn host_target() -> String {
    // Rust 的 `ARCH`/`OS` 与发布目标的命名只差一处：macOS 叫 `macos`，而发布
    // 目标写作 `darwin`。其余（`aarch64`、`x86_64`、`linux`）逐字相同。
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    format!("{os}-{}", std::env::consts::ARCH)
}

/// 当前时间（Unix 秒）。注入点集中在这里，测试可以绕开它。
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 列表里一行：装了什么、选中哪个、授权给哪些项目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledSummary {
    pub plugin: String,
    pub selected: Option<String>,
    pub installed: Vec<String>,
    /// 已授权的项目 id。
    pub projects: Vec<String>,
}

/// 管理服务。只持有实例根，其余状态都在磁盘上。
#[derive(Debug, Clone)]
pub struct PluginService {
    home: PathBuf,
}

impl PluginService {
    pub fn open(instance_home: impl Into<PathBuf>) -> PluginService {
        PluginService {
            home: instance_home.into(),
        }
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    /// 装了什么、选中哪个、授权给谁。
    pub fn list(&self) -> Result<Vec<InstalledSummary>, PluginError> {
        let records = Registry::open(&self.home)?.list()?;
        let table = grants::load(&self.home)?;
        Ok(records
            .into_iter()
            .map(|record| InstalledSummary {
                selected: record.selected.clone(),
                installed: record
                    .installed
                    .iter()
                    .map(|version| version.version.clone())
                    .collect(),
                projects: table
                    .all()
                    .iter()
                    .filter(|grant| grant.plugin == record.plugin.as_str())
                    .map(|grant| grant.project.as_str().to_string())
                    .collect(),
                plugin: record.plugin.as_str().to_string(),
            })
            .collect())
    }

    /// 从**本地**已签名的包安装。
    ///
    /// 与目录下载走同一个 [`PluginService::install_verified_bytes`]：分叉的
    /// 那一天，较弱的那条就是绕过信任的入口。本地导入**不**启用任何项目。
    pub fn install_local(
        &self,
        bundle_path: &Path,
        metadata_path: &Path,
        root_envelope: &[u8],
    ) -> Result<String, PluginError> {
        let archive = std::fs::read(bundle_path)
            .map_err(|e| PluginError::Io(format!("{}: {e}", bundle_path.display())))?;
        let metadata = std::fs::read(metadata_path)
            .map_err(|e| PluginError::Io(format!("{}: {e}", metadata_path.display())))?;
        self.install_verified_bytes(&archive, &metadata, root_envelope)
    }

    /// 从一条受信目录项下载并安装。
    ///
    /// `entry` 来自 `Catalog::find`，不是调用方拼的 URL。下载器只会去
    /// `origin` 允许的地方，跳转每次重验。
    // 七个参数是这条路径的真实输入（目录项、目录、源、取字节的方式、上限、
    // 时钟、信任根）。把其中几个打包成结构体只会让"这到底是哪两个"隐式化，
    // 而这里的每一项都是独立的信任边界，不该被折叠成一个不透明的「配置」。
    #[allow(clippy::too_many_arguments)]
    pub fn install_from_catalog(
        &self,
        entry: &CatalogEntry,
        catalog: &Catalog,
        origin: &AllowedOrigin,
        fetcher: &mut dyn Fetcher,
        limits: &DownloadLimits,
        clock: &mut dyn Clock,
        root_envelope: &[u8],
    ) -> Result<String, PluginError> {
        let (metadata_request, bundle_request) =
            catalog.download_request(&entry.release, origin)?;

        // 先取元数据：它有签名，是"这个包应该是什么"的唯一来源。先下包再验
        // 元数据，就等于按未受信的大小去分配内存。
        let metadata =
            yi_agent_plugins::download(&metadata_request, origin, fetcher, limits, clock)?;
        let bundle = yi_agent_plugins::download(&bundle_request, origin, fetcher, limits, clock)?;

        // 目录说的是不是这条发布：签名验证本身不看目录，这一步把两者对上。
        let mut store = TrustStore::bootstrap(root_envelope)?;
        let verified = store.verify_release(&metadata.bytes, unix_now(), 0)?;
        catalog.agrees_with(&verified)?;

        self.install_verified_bytes(&bundle.bytes, &metadata.bytes, root_envelope)
    }

    /// 把一个**已签名的**包落盘并登记。本地导入与目录下载共用这一条。
    pub fn install_verified_bytes(
        &self,
        archive: &[u8],
        metadata: &[u8],
        root_envelope: &[u8],
    ) -> Result<String, PluginError> {
        let mut store = TrustStore::bootstrap(root_envelope)?;
        let now = unix_now();
        let verified = store.verify_release(metadata, now, 0)?;

        // 包里的清单必须与签名说的一致。签名只覆盖元数据，不覆盖包内容，
        // 所以"包里是什么"要靠这一步把两者对上。
        let manifest_bytes = read_manifest_from_archive(archive)?;
        let manifest = Manifest::parse(&manifest_bytes)?;
        if manifest.plugin != verified.release.plugin
            || manifest.version != verified.release.version
            || manifest.target != verified.release.target
        {
            return Err(PluginError::Integrity(format!(
                "the package manifest describes {}/{}/{} but the signed metadata is {}/{}/{}",
                manifest.plugin.as_str(),
                manifest.version,
                manifest.target,
                verified.release.plugin.as_str(),
                verified.release.version,
                verified.release.target
            )));
        }
        // 签名说它有多大，包里实际就得有多大。
        if verified.size_bytes != archive.len() as u64 {
            return Err(PluginError::Integrity(format!(
                "the package is {} bytes but the signed metadata binds {}",
                archive.len(),
                verified.size_bytes
            )));
        }

        let relative = release_directory(&manifest);
        let destination = self.home.join(&relative);
        if !destination.exists() {
            std::fs::create_dir_all(
                destination
                    .parent()
                    .expect("installation path always has a parent"),
            )?;
            yi_agent_plugins::install_verified(
                &verified,
                &manifest,
                archive,
                &destination,
                &ExtractLimits::default(),
            )?;
        }

        Registry::open(&self.home)?.record_install(
            &manifest.plugin,
            &manifest.version,
            &verified.release.sha256,
            &relative,
            unix_now(),
        )?;

        Ok(format!(
            "installed {} {} ({}) at {}",
            manifest.plugin.as_str(),
            manifest.version,
            manifest.target,
            relative
        ))
    }

    /// 选中版本的完整发布身份。
    ///
    /// target 取自**磁盘上的那份清单**，不猜：注册表只记版本与摘要，
    /// 而 `ReleaseId` 需要三者齐全。
    pub fn selected_release(&self, plugin: &PluginId) -> Result<ReleaseId, PluginError> {
        let record = Registry::open(&self.home)?
            .list()?
            .into_iter()
            .find(|record| &record.plugin == plugin)
            .ok_or_else(|| {
                PluginError::NotFound(format!("plugin `{}` is not registered", plugin.as_str()))
            })?;
        let version = record.selected.clone().ok_or_else(|| {
            PluginError::NotFound(format!(
                "plugin `{}` has no selected version",
                plugin.as_str()
            ))
        })?;
        let installed = record
            .installed
            .iter()
            .find(|installed| installed.version == version)
            .ok_or_else(|| {
                PluginError::NotFound(format!(
                    "plugin `{}` selects `{version}` but it is not installed",
                    plugin.as_str()
                ))
            })?;
        let manifest_path = self
            .home
            .join(&installed.directory)
            .join(managed::MANAGED_MANIFEST);
        let bytes = std::fs::read(&manifest_path)
            .map_err(|e| PluginError::Integrity(format!("{}: {e}", manifest_path.display())))?;
        let manifest = Manifest::parse(&bytes)?;
        Ok(ReleaseId {
            plugin: plugin.clone(),
            version: installed.version.clone(),
            target: manifest.target,
            sha256: installed.sha256.clone(),
        })
    }

    /// 首次为一个项目启用：写一条新授权。
    ///
    /// 这里**不**启动任何进程，也不写 supervisor 清单。启用是"授权"，不是
    /// "安装"，更不是"常驻"。把它做成安装的副作用，会让一次 `install` 悄悄
    /// 产生运行授权。
    pub fn enable_project(
        &self,
        plugin: &PluginId,
        project: ProjectId,
        host: &str,
        capabilities: &[Effect],
    ) -> Result<u64, PluginError> {
        reject_maintenance(capabilities)?;
        let release = self.selected_release(plugin)?;
        let table = grants::update(&self.home, |table| {
            table
                .grant(plugin.as_str(), project, &release, host, capabilities)
                .map(|grant| grant.generation)
        })?;
        Ok(table)
    }

    /// 变更已有授权：能力集合整体替换，代次递增。
    ///
    /// 与 `enable_project` 的差别只在语义：这里要求授权已存在，避免把一次
    /// 拼错的 "change" 变成沉默的"首次启用"。
    pub fn change_grant(
        &self,
        plugin: &PluginId,
        project: &ProjectId,
        host: &str,
        capabilities: &[Effect],
    ) -> Result<u64, PluginError> {
        reject_maintenance(capabilities)?;
        let existing = grants::load(&self.home)?
            .get(plugin.as_str(), project)
            .cloned();
        if existing.is_none() {
            return Err(PluginError::NotFound(format!(
                "plugin `{}` has no grant for `{}`; use enable-project first",
                plugin.as_str(),
                project.as_str()
            )));
        }
        let release = self.selected_release(plugin)?;
        grants::update(&self.home, |table| {
            table
                .grant(
                    plugin.as_str(),
                    project.clone(),
                    &release,
                    host,
                    capabilities,
                )
                .map(|grant| grant.generation)
        })
    }

    /// 停用某个项目（不等于卸载，也不删数据）。
    pub fn disable_project(
        &self,
        plugin: &PluginId,
        project: &ProjectId,
    ) -> Result<u64, PluginError> {
        grants::update(&self.home, |table| table.revoke(plugin.as_str(), project))
    }

    /// 当前已提交的激活。
    pub fn committed(&self, plugin: &PluginId) -> Result<Option<u64>, PluginError> {
        Ok(Lifecycle::open(&self.home)?
            .committed(plugin)?
            .map(|active| active.activation))
    }

    /// 建立一个激活事务。**只建立，不推进。**
    ///
    /// 推进需要 `RuntimeControl`（停旧版本、起候选、健康检查、迁移、提交）。
    /// 那属于运行期控制面；本服务拿不到，于是返回事务 id 并明确说明"尚未
    /// 推进"。假装激活完成，会让调用方以为项目已经在用新版本。
    pub fn activate(&self, plugin: &PluginId, version: &str) -> Result<Activation, PluginError> {
        let release = self.release_for(plugin, version)?;
        let lifecycle = Lifecycle::open(&self.home)?;
        let previous = lifecycle.committed(plugin)?;
        let activation = previous.map(|a| a.activation + 1).unwrap_or(1);
        let request = LifecycleRequest::activate(plugin.clone(), release, activation);
        let transaction = lifecycle.start(request)?;
        Ok(Activation {
            transaction,
            activation,
            advanced: false,
        })
    }

    /// 一个已安装版本的发布身份（按版本号指定）。
    pub fn release_for(&self, plugin: &PluginId, version: &str) -> Result<ReleaseId, PluginError> {
        let record = Registry::open(&self.home)?
            .list()?
            .into_iter()
            .find(|record| &record.plugin == plugin)
            .ok_or_else(|| {
                PluginError::NotFound(format!("plugin `{}` is not registered", plugin.as_str()))
            })?;
        let installed = record
            .installed
            .iter()
            .find(|installed| installed.version == version)
            .ok_or_else(|| {
                PluginError::NotFound(format!(
                    "plugin `{}` has no installed version `{version}`",
                    plugin.as_str()
                ))
            })?;
        let manifest_path = self
            .home
            .join(&installed.directory)
            .join(managed::MANAGED_MANIFEST);
        let bytes = std::fs::read(&manifest_path)
            .map_err(|e| PluginError::Integrity(format!("{}: {e}", manifest_path.display())))?;
        let manifest = Manifest::parse(&bytes)?;
        Ok(ReleaseId {
            plugin: plugin.clone(),
            version: installed.version.clone(),
            target: manifest.target,
            sha256: installed.sha256.clone(),
        })
    }

    /// 当前打开的事务。
    pub fn transaction(&self, plugin: &PluginId) -> Result<Option<TransactionView>, PluginError> {
        let store = yi_agent_plugins::JournalStore::open(&self.home)?;
        Ok(store.load(plugin)?.map(|journal| TransactionView {
            transaction: journal.transaction_id,
            phase: journal.phase.as_str().to_string(),
            admission_open: journal.phase.admission_open(),
            rollbackable: journal.phase.rollbackable(),
        }))
    }

    /// 取消一个事务。
    ///
    /// 只在可安全退回的阶段允许；`projecting` 之后提交已经发生，回退就是
    /// 第二次提交。真正的回滚动作需要 `RuntimeControl`，本服务只做前置判断。
    pub fn cancel_transaction(&self, plugin: &PluginId) -> Result<CancelOutcome, PluginError> {
        let store = yi_agent_plugins::JournalStore::open(&self.home)?;
        let journal = store.load(plugin)?.ok_or_else(|| {
            PluginError::NotFound(format!(
                "plugin `{}` has no open transaction",
                plugin.as_str()
            ))
        })?;
        if !journal.phase.rollbackable() {
            return Err(PluginError::RecoveryRequired(format!(
                "transaction `{}` is at phase {}; undoing it here would be a second commit",
                journal.transaction_id,
                journal.phase.as_str()
            )));
        }
        Ok(CancelOutcome {
            transaction: journal.transaction_id,
            phase: journal.phase.as_str().to_string(),
        })
    }

    /// 卸载。默认保留用户数据；删数据是独立的一次显式动作。
    pub fn uninstall(
        &self,
        plugin: &PluginId,
        remove_data: bool,
    ) -> Result<managed_uninstall::UninstallOutcome, PluginError> {
        managed_uninstall::uninstall(&self.home, plugin, remove_data)
    }

    /// 只清理引用安全的受管产物。
    pub fn gc(&self) -> Result<managed_uninstall::GcOutcome, PluginError> {
        managed_uninstall::gc(&self.home)
    }

    /// 把一个项目路径规整为授权域 id。
    ///
    /// 不接受任意字符串当项目 id：允许的话 `/a/repo` 与 `/a/repo/../repo`
    /// 会变成两个授权域。
    pub fn resolve_project(&self, raw: &str) -> Result<ProjectId, PluginError> {
        let mut resolver = ProjectResolver::new();
        resolver
            .register(Path::new(raw))
            .map(|root| root.id)
            .map_err(|error| PluginError::InvalidManifest(format!("{error}")))
    }

    /// 宿主身份。缺省取 supervisor 的实例级 id。
    pub fn host_id(&self) -> Result<String, PluginError> {
        managed::host_id(&self.home)
    }
}

/// `activate` 的结果：事务已建立，但推进由 daemon 负责。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activation {
    pub transaction: String,
    pub activation: u64,
    /// 本服务是否推进了它。恒为 `false`——推进属于运行期控制面。
    pub advanced: bool,
}

/// `transaction` 的只读视图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransactionView {
    pub transaction: String,
    pub phase: String,
    pub admission_open: bool,
    pub rollbackable: bool,
}

/// `cancel-transaction` 的前置判断结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelOutcome {
    pub transaction: String,
    pub phase: String,
}

/// `Maintenance` 只能由生命周期事务签发，插件不能自己申请。
fn reject_maintenance(capabilities: &[Effect]) -> Result<(), PluginError> {
    if capabilities.contains(&Effect::Maintenance) {
        return Err(PluginError::PermissionDenied(
            "`maintenance` is issued only by a lifecycle transaction; a plugin may not request it"
                .to_string(),
        ));
    }
    Ok(())
}

/// 从包里读 `plugin.json`。
///
/// 用契约核心的归档读取器而不是自己再解一遍 tar：两次实现迟早分叉，而
/// "宿主按 A 的规则读、安装按 B 的规则写"正是签名验证被绕过的经典形状。
fn read_manifest_from_archive(archive: &[u8]) -> Result<Vec<u8>, PluginError> {
    yi_agent_plugins::bundle::read_package_file(archive, managed::MANAGED_MANIFEST, 1024 * 1024)
}

/// 一个能力字符串 → `Effect`。
pub fn parse_capability(raw: &str) -> Result<Effect, PluginError> {
    match raw {
        "read" => Ok(Effect::Read),
        "write" => Ok(Effect::Write),
        "launch" => Ok(Effect::Launch),
        "quota-write" => Ok(Effect::QuotaWrite),
        "resident" => Ok(Effect::Resident),
        "maintenance" => Err(PluginError::PermissionDenied(
            "`maintenance` is issued only by a lifecycle transaction; a plugin may not request it"
                .to_string(),
        )),
        other => Err(PluginError::InvalidManifest(format!(
            "unknown capability `{other}`; expected read|write|launch|quota-write|resident"
        ))),
    }
}

/// 供 RPC 校验一个 `Binding` 是否仍然有效。
pub fn binding_state(instance_home: &Path, binding: &Binding) -> Result<bool, PluginError> {
    let lifecycle = Lifecycle::open(instance_home)?;
    let committed = lifecycle.committed(&binding.release.plugin)?;
    Ok(committed
        .map(|active| active.activation == binding.activation)
        .unwrap_or(false))
}

#[cfg(test)]
mod tests;
#[cfg(test)]
pub(crate) mod tests_support;
