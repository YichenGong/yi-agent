//! `yi-agent plugins` 子命令：受管执行与管理入口。
//!
//! # 两条完全不同的路
//!
//! - [`exec`] 起一个**受管**插件进程：身份、授权、准入、完整性全部检查过，
//!   程序路径来自注册表而不是用户 `PATH`。
//! - 其余子命令是**管理**操作：作用在实例状态上，不携带任何页面/业务权限。
//!   它们不需要桌面在跑，所以 headless 也能完成安装与更新。
//!
//! # 未实现的必须说"未实现"
//!
//! 管理表里有若干条目依赖尚未落地的下游任务（下载渠道、发布目录、
//! 旧安装探测）。这些一律返回 `NotSupported`，**不伪装成功**：一次假的
//! "已安装"会让调用方跳过真正的校验，而校验正是这一层的全部价值。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use yi_agent_plugins::{
    Effect, Lifecycle, LifecycleRequest, Manifest, PluginError, PluginId, ProjectId, Registry,
    ReleaseId, project::ProjectResolver,
};
use yi_agent_supervisors::managed::{self, LaunchRequest, grants};

use crate::config::PluginsAction;

/// 实例根。`YI_AGENT_HOME` 优先，与 daemon / app-server 同一份解析。
fn instance_home() -> Result<PathBuf> {
    yi_agent_store::paths::instance_home()
        .context("no instance root: set YI_AGENT_HOME or make sure HOME exists")
}

fn plugin_id(raw: &str) -> Result<PluginId> {
    PluginId::parse(raw).map_err(|error| anyhow::anyhow!("{error}"))
}

fn project_id(raw: &str) -> Result<ProjectId> {
    ProjectId::parse(raw).map_err(|error| anyhow::anyhow!("{error}"))
}

/// 默认宿主身份。
fn host() -> Result<String> {
    managed::host_id(&instance_home()?).map_err(|error| anyhow::anyhow!("{error}"))
}

/// 把 `PluginError` 转成 CLI 的退出码。
///
/// 统一在这里翻译，是为了让每个子命令的错误都带稳定的 `kind` 前缀：
/// 自动化脚本按 kind 分支，读一段人类可读的 message 不可靠。
fn report(error: PluginError) -> i32 {
    eprintln!("error [{}]: {error}", error.kind());
    1
}

/// `yi-agent plugins <action>`
pub fn run(action: &PluginsAction) -> Result<i32> {
    match action {
        PluginsAction::Exec {
            plugin,
            project_handle,
            binding_file,
            argv,
        } => exec(plugin, project_handle, binding_file, argv),
        PluginsAction::Maintenance { plugin } => maintenance(plugin),
        PluginsAction::Catalog => not_supported("catalog", "the catalog channel ships in T16"),
        PluginsAction::List => list(),
        PluginsAction::Install {
            release,
            bundle,
            metadata,
        } => install(release.as_deref(), bundle.as_deref(), metadata.as_deref()),
        PluginsAction::EnableProject {
            plugin,
            project,
            capability,
            host_id,
        } => enable_project(plugin, project, capability, host_id.as_deref()),
        PluginsAction::ChangeGrant {
            plugin,
            project,
            capability,
            host_id,
        } => change_grant(plugin, project, capability, host_id.as_deref()),
        PluginsAction::DisableProject { plugin, project } => disable_project(plugin, project),
        PluginsAction::Activate { plugin, version } => activate(plugin, version),
        PluginsAction::Rollback { plugin } => rollback(plugin),
        PluginsAction::Transaction { plugin } => transaction(plugin),
        PluginsAction::CancelTransaction { plugin } => cancel_transaction(plugin),
        PluginsAction::Uninstall => {
            not_supported("uninstall", "removal lands with the T16 catalog channel")
        }
        PluginsAction::ClearData => not_supported(
            "clear-data",
            "data clearing lands with the T16 catalog channel",
        ),
        PluginsAction::Gc => not_supported(
            "gc",
            "garbage collection lands with the T16 catalog channel",
        ),
        PluginsAction::InspectLegacy => {
            not_supported("inspect-legacy", "legacy detection lands in T25")
        }
        PluginsAction::AdoptLegacy => not_supported("adopt-legacy", "legacy adoption lands in T25"),
    }
}

/// 查询维护屏障。
///
/// # 为什么先问 daemon 再自己读盘
///
/// daemon 是**唯一**裁决"能不能启动后端"的一方。如果 CLI 自己读盘给一个
/// 结论，用户在 daemon 有不同看法时（例如它正推进事务）会得到互相矛盾的
/// 两个答案。所以：daemon 在跑就问它；daemon 没跑就直接读盘——那时也没有
/// 谁在启动后端，单纯的诊断结论是正确的。
///
/// 两者结论都来自同一份磁盘记录，因此"daemon 不在"不会退化成更宽松的判断。
fn maintenance(plugin: &str) -> Result<i32> {
    let plugin = plugin_id(plugin)?;
    let home = instance_home()?;

    let barrier = yi_agent_store::ipc::MaintenanceBarrier::at(&home);
    let local = barrier.state(&plugin);

    // 问 daemon：让用户看到它此刻的裁决。查询失败**不**改变结论，
    // 只说明"没有 daemon 在听"。
    let socket = yi_agent_store::ipc::socket_path_for(&maintenance_runtime_dir()?)?;
    let remote = yi_agent_store::ipc::send_request(
        &socket,
        yi_agent_store::ipc::IpcRequest::PluginMaintenance {
            plugin: plugin.as_str().to_string(),
            protocol: Some(yi_agent_store::ipc::MAINTENANCE_PROTOCOL_VERSION),
        },
    );

    println!("plugin: {}", plugin.as_str());
    println!("state: {}", local.state.as_str());
    if let Some(transaction) = &local.transaction_id {
        println!("transaction: {transaction}");
    }
    if let Some(phase) = &local.phase {
        println!("phase: {phase}");
    }
    if let Some(detail) = &local.detail {
        println!("detail: {detail}");
    }
    match remote {
        Ok(yi_agent_store::ipc::IpcResponse::PluginMaintenanceStatus { state, .. }) => {
            println!("daemon: {state}");
        }
        Ok(other) => println!("daemon: unexpected answer {other:?}"),
        Err(_) => println!("daemon: not running"),
    }

    // 退出码与结论绑定：脚本可以直接用。`open` 是 0，其余都是 1——
    // 把 `unknown` 也算作"不能启动"，否则一条 `&&` 链会把它当放行。
    Ok(if local.state.admits_launch() { 0 } else { 1 })
}

/// 维护查询用的 runtime 目录。
///
/// 屏障本身是实例级的（只读 `instance_home` 下的日志），但**问谁**是
/// 项目级的：daemon 按工作目录各起一个，socket 落在
/// `<workdir>/.yi-agent/runtime`。用错目录只会得到"daemon: not running"，
/// 那仍然给出正确的本地结论——但会让用户以为 daemon 没跑。
fn maintenance_runtime_dir() -> Result<PathBuf> {
    // 与 `main` 走同一条解析：默认取 cwd，`--workdir` 可覆盖。不自己讲一套
    // 规则，否则 CLI 与 daemon 会在不同目录上各说各话。
    let workdir = std::env::current_dir().context("no current directory")?;
    Ok(yi_agent_subagent::attach::project_runtime_directory(
        &workdir,
    ))
}

fn not_supported(method: &str, why: &str) -> Result<i32> {
    eprintln!("error [NotSupported]: {method} is not implemented yet: {why}");
    Ok(1)
}

/// `plugins exec`
///
/// argv 在 CLI 层就已经是 `Vec<OsString>`（`trailing_var_arg`），到这里的
/// 每一步都不经过 shell：空格、中文、分号都只是字符。
fn exec(
    plugin: &str,
    project_handle: &str,
    binding_file: &Path,
    argv: &[std::ffi::OsString],
) -> Result<i32> {
    let home = instance_home()?;
    let plugin = plugin_id(plugin)?;
    let project = project_id(project_handle)?;

    let binding = read_binding(binding_file)?;
    if binding.project != project {
        return Ok(report(PluginError::PermissionDenied(format!(
            "binding authorizes project `{}`, not `{project_handle}`",
            binding.project.as_str()
        ))));
    }

    let install = match managed::resolve(&home, &plugin) {
        Ok(install) => install,
        Err(error) => return Ok(report(error)),
    };
    let table = match grants::load(&home) {
        Ok(table) => table,
        Err(error) => return Ok(report(error)),
    };

    let workdir = home
        .parent()
        .map(|parent| parent.to_path_buf())
        .unwrap_or_else(|| home.clone());
    // 插件声明的环境项在 v1 清单里没有落点（`manifest-v1.schema.json` 没有
    // env 字段）。**不能**把宿主自己的 `YI_AGENT_PLUGIN_*` 当成"插件项"传进来：
    // 那些正是保留变量，会让每次 exec 都被自己的检查拒绝。
    let plugin_env: Vec<(String, String)> = Vec::new();

    let request = LaunchRequest {
        mode: managed::LaunchMode::Cli,
        instance_home: &home,
        install: &install,
        binding: &binding,
        workdir: &workdir,
        argv,
        plugin_env: &plugin_env,
        grants: &table,
    };
    let mut command = match managed::prepare(&request) {
        Ok(command) => command,
        Err(error) => return Ok(report(error)),
    };

    // 子进程继承本进程的终端：`exec` 是前台命令，输出必须让用户看见。
    command.stdout(std::process::Stdio::inherit());
    command.stderr(std::process::Stdio::inherit());
    let status = command.status().with_context(|| {
        format!(
            "could not launch plugin `{}`",
            install.manifest.plugin.as_str()
        )
    })?;
    Ok(status.code().unwrap_or(1))
}

/// 绑定文件由宿主生成。**不接受页面上传的文件作为授权**：授权来自这里
/// 读到的绑定，而绑定必须由宿主绑定到当前会话与宿主身份。
fn read_binding(path: &Path) -> Result<yi_agent_plugins::Binding> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("could not read the binding file {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("binding file {} is not a valid binding", path.display()))
}

/// `plugins list`
fn list() -> Result<i32> {
    let home = instance_home()?;
    let records = match Registry::open(&home) {
        Ok(registry) => registry.list(),
        Err(error) => return Ok(report(error)),
    };
    let records = match records {
        Ok(records) => records,
        Err(error) => return Ok(report(error)),
    };
    let table = match grants::load(&home) {
        Ok(table) => table,
        Err(error) => return Ok(report(error)),
    };
    for record in records {
        let selected = record.selected.as_deref().unwrap_or("-");
        let versions = record
            .installed
            .iter()
            .map(|version| version.version.as_str())
            .collect::<Vec<_>>()
            .join(",");
        let authorized: Vec<String> = table
            .all()
            .iter()
            .filter(|grant| grant.plugin == record.plugin.as_str())
            .map(|grant| grant.project.as_str().to_string())
            .collect();
        println!(
            "{}\tselected={selected}\tinstalled=[{versions}]\tprojects=[{}]",
            record.plugin.as_str(),
            authorized.join(",")
        );
    }
    Ok(0)
}

/// `plugins install`
///
/// 二选一：官方目录的 release 身份，或本地 `--bundle` + `--metadata`。
/// 两条路都走**同一个**验证流程（T07/T08 的 `verify_release` +
/// `install_verified`）。这里只保留本地那条，因为目录渠道属于 T16。
fn install(release: Option<&str>, bundle: Option<&Path>, metadata: Option<&Path>) -> Result<i32> {
    let (Some(bundle), Some(metadata)) = (bundle, metadata) else {
        if release.is_some() {
            return not_supported(
                "install --release",
                "the official catalog channel ships in T16; use --bundle/--metadata",
            );
        }
        eprintln!("error: install needs either --release or both --bundle and --metadata");
        return Ok(1);
    };

    let home = instance_home()?;
    match install_local(&home, bundle, metadata) {
        Ok(report) => {
            println!("{}", report);
            Ok(0)
        }
        Err(error) => Ok(report(error)),
    }
}

fn install_local(
    home: &Path,
    bundle_path: &Path,
    metadata_path: &Path,
) -> Result<String, PluginError> {
    use yi_agent_plugins::{ExtractLimits, TrustStore, install_verified};

    let archive = std::fs::read(bundle_path)
        .map_err(|e| PluginError::Io(format!("{}: {e}", bundle_path.display())))?;
    let metadata = std::fs::read(metadata_path)
        .map_err(|e| PluginError::Io(format!("{}: {e}", metadata_path.display())))?;

    // 信任根来自仓库随发布附带的 golden 夹具；生产路径由 T16 换成随包分发的根。
    let root = std::fs::read(root_envelope_path()).map_err(|e| {
        PluginError::Untrusted(format!(
            "no trust root available at {}: {e}",
            root_envelope_path().display()
        ))
    })?;
    let mut store = TrustStore::bootstrap(&root)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let verified = store.verify_release(&metadata, now, 0)?;

    let manifest_bytes = read_manifest_from_archive(&archive)?;
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

    let relative = format!(
        "plugins/installed/{}/{}/{}",
        manifest.plugin.as_str(),
        manifest.version,
        manifest.target
    );
    let destination = home.join(&relative);
    if !destination.exists() {
        std::fs::create_dir_all(
            destination
                .parent()
                .expect("installation path always has a parent"),
        )?;
        install_verified(
            &verified,
            &manifest,
            &archive,
            &destination,
            &ExtractLimits::default(),
        )?;
    }

    let registry = Registry::open(home)?;
    let installed_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    registry.record_install(
        &manifest.plugin,
        &manifest.version,
        &verified.release.sha256,
        &relative,
        installed_at,
    )?;

    Ok(format!(
        "installed {} {} ({}) at {}",
        manifest.plugin.as_str(),
        manifest.version,
        manifest.target,
        relative
    ))
}

fn root_envelope_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("plugins/contracts/fixtures/root-v1.json")
}

/// 从包里读 `plugin.json`。
///
/// 用插件契约核心的归档读取器而不是自己再解一遍 tar：两次实现迟早会分叉，
/// 而"宿主按 A 的规则读、安装按 B 的规则写"正是签名验证被绕过的经典形状。
fn read_manifest_from_archive(archive: &[u8]) -> Result<Vec<u8>, PluginError> {
    // 1 MiB 对一份清单绰绰有余；给它设界是为了让"包很大"不会变成
    // "先把整个条目读进内存再说"。
    yi_agent_plugins::bundle::read_package_file(archive, managed::MANAGED_MANIFEST, 1024 * 1024)
}

/// 有效的能力列表，按 `(plugin, project)` 写入新的授权。
fn enable_project(
    plugin: &str,
    project: &str,
    capability: &[String],
    host_override: Option<&str>,
) -> Result<i32> {
    let home = instance_home()?;
    let host = match host_override {
        Some(host) => host.to_string(),
        None => host()?,
    };
    let plugin_id = plugin_id(plugin)?;
    let project = resolve_project(project)?;
    let effects = capabilities(capability)?;

    let release = match Registry::open(&home) {
        Ok(registry) => registry.list(),
        Err(error) => return Ok(report(error)),
    };
    let selected = match release {
        Ok(records) => records
            .into_iter()
            .find(|record| record.plugin == plugin_id)
            .and_then(|record| {
                let selected = record.selected.clone()?;
                record
                    .installed
                    .into_iter()
                    .find(|version| version.version == selected)
            }),
        Err(error) => return Ok(report(error)),
    };
    let Some(selected) = selected else {
        return Ok(report(PluginError::NotFound(format!(
            "plugin `{plugin}` has no selected version; install one first"
        ))));
    };
    let release = ReleaseId {
        plugin: plugin_id.clone(),
        version: selected.version,
        target: manifest_target(&home, &plugin_id)?,
        sha256: selected.sha256,
    };

    let outcome = grants::update(&home, |table| {
        table
            .grant(
                plugin_id.as_str(),
                project.clone(),
                &release,
                &host,
                &effects,
            )
            .map(|grant| grant.generation)
    });
    match outcome {
        Ok(generation) => {
            println!(
                "enabled {} for {} (generation {generation})",
                plugin_id.as_str(),
                project.as_str()
            );
            Ok(0)
        }
        Err(error) => Ok(report(error)),
    }
}

/// 变更已有授权：能力集合整体替换，代次递增。
fn change_grant(
    plugin: &str,
    project: &str,
    capability: &[String],
    host_override: Option<&str>,
) -> Result<i32> {
    // 与 enable-project 的差别只在语义：这里要求授权已存在，避免把一次
    // 拼错的 "change" 变成沉默的 "首次启用"。
    let home = instance_home()?;
    let plugin_id = plugin_id(plugin)?;
    let project = resolve_project(project)?;
    let effects = capabilities(capability)?;
    let host = match host_override {
        Some(host) => host.to_string(),
        None => host()?,
    };

    let existing = match grants::load(&home) {
        Ok(table) => table.get(plugin_id.as_str(), &project).cloned(),
        Err(error) => return Ok(report(error)),
    };
    let Some(_existing) = existing else {
        return Ok(report(PluginError::NotFound(format!(
            "plugin `{plugin}` has no grant for `{}`; use enable-project first",
            project.as_str()
        ))));
    };

    let release = match selected_release(&home, &plugin_id) {
        Ok(release) => release,
        Err(error) => return Ok(report(error)),
    };
    let outcome = grants::update(&home, |table| {
        table
            .grant(
                plugin_id.as_str(),
                project.clone(),
                &release,
                &host,
                &effects,
            )
            .map(|grant| grant.generation)
    });
    match outcome {
        Ok(generation) => {
            println!(
                "changed grant for {} in {} (generation {generation})",
                plugin_id.as_str(),
                project.as_str()
            );
            Ok(0)
        }
        Err(error) => Ok(report(error)),
    }
}

fn disable_project(plugin: &str, project: &str) -> Result<i32> {
    let home = instance_home()?;
    let plugin_id = plugin_id(plugin)?;
    let project = resolve_project(project)?;
    let outcome = grants::update(&home, |table| table.revoke(plugin_id.as_str(), &project));
    match outcome {
        Ok(generation) => {
            println!(
                "disabled {} for {} (generation {generation})",
                plugin_id.as_str(),
                project.as_str()
            );
            Ok(0)
        }
        Err(error) => Ok(report(error)),
    }
}

/// 项目规整：路径必须存在，映射到规范路径构成的 `ProjectId`。
///
/// 不接受任意字符串当项目 id。设计里项目身份就是规范化路径：允许任意
/// 字符串会让 `/a/repo` 与 `/a/repo/../repo` 变成两个授权域。
fn resolve_project(raw: &str) -> Result<ProjectId> {
    let mut resolver = ProjectResolver::new();
    let root = resolver
        .register(Path::new(raw))
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    Ok(root.id)
}

fn capabilities(raw: &[String]) -> Result<Vec<Effect>> {
    raw.iter()
        .map(|value| match value.as_str() {
            "read" => Ok(Effect::Read),
            "write" => Ok(Effect::Write),
            "launch" => Ok(Effect::Launch),
            "quota-write" => Ok(Effect::QuotaWrite),
            "resident" => Ok(Effect::Resident),
            "maintenance" => Err(anyhow::anyhow!(
                "`maintenance` is issued only by a lifecycle transaction; a plugin may not request it"
            )),
            other => Err(anyhow::anyhow!(
                "unknown capability `{other}`; expected read|write|launch|quota-write|resident"
            )),
        })
        .collect()
}

fn manifest_target(home: &Path, plugin: &PluginId) -> Result<String, PluginError> {
    Ok(selected_release(home, plugin)?.target)
}

fn selected_release(home: &Path, plugin: &PluginId) -> Result<ReleaseId, PluginError> {
    let record = Registry::open(home)?
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
    // target 取自安装在磁盘上的那份清单，而不是猜。
    let manifest_path = home
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

/// 激活一个已安装版本。
///
/// 激活是一个**事务**：`Lifecycle::start` 只建立它，推进需要宿主提供
/// `RuntimeControl`。CLI 没有运行期控制面（那属于 daemon），所以这里
/// 完成后必须明确报告"事务已建立、尚未推进"，而不是假装激活完成。
fn activate(plugin: &str, version: &str) -> Result<i32> {
    let home = instance_home()?;
    let plugin = plugin_id(plugin)?;
    let release = match release_for(&home, &plugin, version) {
        Ok(release) => release,
        Err(error) => return Ok(report(error)),
    };

    let lifecycle = match Lifecycle::open(&home) {
        Ok(lifecycle) => lifecycle,
        Err(error) => return Ok(report(error)),
    };
    let previous = match lifecycle.committed(&plugin) {
        Ok(previous) => previous,
        Err(error) => return Ok(report(error)),
    };
    let activation = previous.map(|active| active.activation + 1).unwrap_or(1);

    let request = LifecycleRequest::activate(plugin.clone(), release, activation);
    match lifecycle.start(request) {
        Ok(transaction) => {
            println!(
                "started activation transaction {transaction} for {} (generation {activation})",
                plugin.as_str()
            );
            println!(
                "note: the transaction is not advanced by this command; the daemon drives \
                 draining, migration and health checks"
            );
            Ok(0)
        }
        Err(error) => Ok(report(error)),
    }
}

fn rollback(plugin: &str) -> Result<i32> {
    let home = instance_home()?;
    let plugin = plugin_id(plugin)?;
    let journal = match yi_agent_plugins::JournalStore::open(&home) {
        Ok(store) => store.load(&plugin),
        Err(error) => return Ok(report(error)),
    };
    let journal = match journal {
        Ok(journal) => journal,
        Err(error) => return Ok(report(error)),
    };
    let Some(journal) = journal else {
        return Ok(report(PluginError::NotFound(format!(
            "plugin `{}` has no open transaction to roll back",
            plugin.as_str()
        ))));
    };
    if !journal.phase.rollbackable() {
        return Ok(report(PluginError::RecoveryRequired(format!(
            "transaction `{}` is at phase {}; rolling back after `projecting` would be a second \
             commit, so it must be repaired instead",
            journal.transaction_id,
            journal.phase.as_str()
        ))));
    }
    not_supported(
        "rollback",
        "needs RuntimeControl to stop the candidate and restore admission; that is the daemon's job",
    )
}

fn transaction(plugin: &str) -> Result<i32> {
    let home = instance_home()?;
    let plugin = plugin_id(plugin)?;
    let journal = match yi_agent_plugins::JournalStore::open(&home) {
        Ok(store) => store.load(&plugin),
        Err(error) => return Ok(report(error)),
    };
    match journal {
        Ok(Some(journal)) => {
            println!(
                "{}\t{}\t{}\tadmission={}",
                journal.transaction_id,
                journal.plugin.as_str(),
                journal.phase.as_str(),
                if journal.phase.admission_open() {
                    "open"
                } else {
                    "closed"
                }
            );
            Ok(0)
        }
        Ok(None) => {
            println!("no open transaction");
            Ok(0)
        }
        Err(error) => Ok(report(error)),
    }
}

fn cancel_transaction(plugin: &str) -> Result<i32> {
    let home = instance_home()?;
    let plugin = plugin_id(plugin)?;
    let journal = match yi_agent_plugins::JournalStore::open(&home) {
        Ok(store) => store.load(&plugin),
        Err(error) => return Ok(report(error)),
    };
    let journal = match journal {
        Ok(journal) => journal,
        Err(error) => return Ok(report(error)),
    };
    let Some(journal) = journal else {
        return Ok(report(PluginError::NotFound(format!(
            "plugin `{}` has no open transaction",
            plugin.as_str()
        ))));
    };
    if !journal.phase.rollbackable() {
        return Ok(report(PluginError::RecoveryRequired(format!(
            "transaction `{}` is at phase {}, past the point where a cancel is safe",
            journal.transaction_id,
            journal.phase.as_str()
        ))));
    }
    not_supported(
        "cancel-transaction",
        "cancelling restores the previous activation through RuntimeControl; that is the daemon's job",
    )
}

fn release_for(home: &Path, plugin: &PluginId, version: &str) -> Result<ReleaseId, PluginError> {
    let record = Registry::open(home)?
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
    let manifest_path = home
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_reject_maintenance() {
        // Maintenance 只由生命周期事务签发。CLI 若能直接授予它，管理面就
        // 多了一条"自己给自己开维护能力"的旁路。
        let error = capabilities(&["maintenance".to_string()])
            .expect_err("a plugin may not request maintenance");
        assert!(error.to_string().contains("lifecycle transaction"));
    }

    #[test]
    fn an_unknown_capability_is_rejected_rather_than_ignored() {
        let error = capabilities(&["read".to_string(), "everything".to_string()])
            .expect_err("an unknown capability must not be silently dropped");
        assert!(error.to_string().contains("unknown capability"));
    }

    #[test]
    fn capabilities_map_to_the_declared_effects() {
        let effects = capabilities(&[
            "read".to_string(),
            "write".to_string(),
            "launch".to_string(),
            "quota-write".to_string(),
            "resident".to_string(),
        ])
        .unwrap();
        assert_eq!(
            effects,
            vec![
                Effect::Read,
                Effect::Write,
                Effect::Launch,
                Effect::QuotaWrite,
                Effect::Resident
            ]
        );
    }
}
