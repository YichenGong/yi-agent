//! `yi-agent plugins` 子命令：受管执行与管理入口。
//!
//! # 两条完全不同的路
//!
//! - [`exec`] 起一个**受管**插件进程：身份、授权、准入、完整性全部检查过，
//!   程序路径来自注册表而不是用户 `PATH`。
//! - 其余子命令是**管理**操作：作用在实例状态上，不携带任何页面/业务权限。
//!   它们不需要桌面在跑，所以 headless 也能完成安装与更新。
//!
//! # 本文件只负责"变成调用、变成输出"
//!
//! 管理逻辑全部在 [`yi_agent_app_server::plugin_manager::service::PluginService`]。
//! 桌面 RPC（`plugins/v1/*`）用的是同一个实现。这里复制一份的日子就是两份
//! 实现开始分叉的日子，而它们读写的是同一组权威记录（registry、grants、
//! lifecycle）——分叉的较弱那份就是绕过校验的入口。
//!
//! # 未实现的必须说"未实现"
//!
//! 依赖尚未落地下游任务的方法一律返回 `NotSupported`，**不伪装成功**：
//! 一次假的"已安装"会让调用方跳过真正的校验，而校验正是这一层的全部价值。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use yi_agent_app_server::plugin_manager::service::{self, PluginService};
use yi_agent_plugins::{PluginError, PluginId, ReleaseId};
use yi_agent_supervisors::managed::{self, LaunchRequest, grants};

use crate::config::PluginsAction;

/// 实例根。`YI_AGENT_HOME` 优先，与 daemon / app-server 同一份解析。
fn instance_home() -> Result<PathBuf> {
    yi_agent_store::paths::instance_home()
        .context("no instance root: set YI_AGENT_HOME or make sure HOME exists")
}

fn service() -> Result<PluginService> {
    Ok(PluginService::open(instance_home()?))
}

fn plugin_id(raw: &str) -> Result<PluginId> {
    PluginId::parse(raw).map_err(|error| anyhow::anyhow!("{error}"))
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
        PluginsAction::Catalog => catalog(),
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
        PluginsAction::Uninstall { plugin } => uninstall_plugin(plugin, false),
        PluginsAction::ClearData {
            plugin,
            projects,
            yes,
        } => clear_data(plugin, projects, *yes),
        PluginsAction::Gc => gc(),
        PluginsAction::InspectLegacy => {
            not_supported("inspect-legacy", "legacy detection lands in T25")
        }
        PluginsAction::AdoptLegacy => not_supported("adopt-legacy", "legacy adoption lands in T25"),
    }
}

// ---------------------------------------------------------------------------
// 管理子命令：全部委托给 PluginService
// ---------------------------------------------------------------------------

/// `plugins catalog`
///
/// 目录渠道的入口是"目录里有什么"。它**不**安装、不启用、不激活任何东西：
/// 一个查询不该有副作用。
fn catalog() -> Result<i32> {
    // 目录内容不是信任凭据：它只说明"官方源上有这么个版本"。真正的授权
    // 仍来自每一个包的签名。源的认证由"钉死的 https 主机名"提供，故这里
    // 先校验源配置，再展示条目。
    if let Err(error) = catalog_origin() {
        return Ok(report(error));
    }
    let catalog = match load_catalog() {
        Ok(catalog) => catalog,
        Err(error) => return Ok(report(error)),
    };
    for entry in &catalog.entries {
        println!(
            "{}\t{}\t{}\t{}",
            entry.release.plugin.as_str(),
            entry.release.version,
            entry.release.target,
            entry.release.sha256
        );
    }
    Ok(0)
}

/// `plugins list`
fn list() -> Result<i32> {
    let service = service()?;
    let summaries = match service.list() {
        Ok(summaries) => summaries,
        Err(error) => return Ok(report(error)),
    };
    for summary in summaries {
        let selected = summary.selected.as_deref().unwrap_or("-");
        println!(
            "{}\tselected={selected}\tinstalled=[{}]\tprojects=[{}]",
            summary.plugin,
            summary.installed.join(","),
            summary.projects.join(",")
        );
    }
    Ok(0)
}

/// `plugins install`
///
/// 二选一：官方目录的 release 身份，或本地 `--bundle` + `--metadata`。
/// 两条路都走**同一个** `PluginService::install_verified_bytes`。
fn install(release: Option<&str>, bundle: Option<&Path>, metadata: Option<&Path>) -> Result<i32> {
    let service = service()?;
    let root = match root_envelope() {
        Ok(root) => root,
        Err(error) => return Ok(report(error)),
    };

    let (Some(bundle), Some(metadata)) = (bundle, metadata) else {
        if let Some(spec) = release {
            return install_from_catalog(&service, spec, &root);
        }
        eprintln!("error: install needs either --release or both --bundle and --metadata");
        return Ok(1);
    };

    match service.install_local(bundle, metadata, &root) {
        Ok(line) => {
            println!("{line}");
            Ok(0)
        }
        Err(error) => Ok(report(error)),
    }
}

/// 目录安装：目录给路径，源由宿主钉死。
///
/// 发布身份由调用方给出（`plugin@version#sha`），而不是从目录里"猜最近一个"：
/// 一次安装在身份上是明确的，否则用户点"安装"之后装到的可能不是他看的那条。
fn install_from_catalog(service: &PluginService, spec: &str, root: &[u8]) -> Result<i32> {
    let origin = match catalog_origin() {
        Ok(origin) => origin,
        Err(error) => return Ok(report(error)),
    };
    let release = match parse_release_spec(spec) {
        Ok(release) => release,
        Err(error) => return Ok(report(error)),
    };
    let catalog = match load_catalog() {
        Ok(catalog) => catalog,
        Err(error) => return Ok(report(error)),
    };
    let entry = match catalog.find(&release) {
        Some(entry) => entry.clone(),
        None => {
            return Ok(report(PluginError::NotFound(format!(
                "the official catalog has no {} {} for {}",
                release.plugin.as_str(),
                release.version,
                release.target
            ))));
        }
    };

    let mut fetcher = CurlFetcher::default();
    let limits = yi_agent_plugins::DownloadLimits::default();
    let mut clock = MonotonicClock::new();
    match service.install_from_catalog(
        &entry,
        &catalog,
        &origin,
        &mut fetcher,
        &limits,
        &mut clock,
        root,
    ) {
        Ok(line) => {
            println!("{line}");
            Ok(0)
        }
        Err(error) => Ok(report(error)),
    }
}

/// `plugins enable-project`
fn enable_project(
    plugin: &str,
    project: &str,
    capability: &[String],
    host_override: Option<&str>,
) -> Result<i32> {
    let service = service()?;
    let plugin_id = plugin_id(plugin)?;
    let host = match host_override {
        Some(host) => host.to_string(),
        None => match service.host_id() {
            Ok(host) => host,
            Err(error) => return Ok(report(error)),
        },
    };
    let project = match service.resolve_project(project) {
        Ok(project) => project,
        Err(error) => return Ok(report(error)),
    };
    let effects = match capabilities(capability) {
        Ok(effects) => effects,
        Err(error) => return Ok(report(error)),
    };
    match service.enable_project(&plugin_id, project.clone(), &host, &effects) {
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

/// `plugins change-grant`
fn change_grant(
    plugin: &str,
    project: &str,
    capability: &[String],
    host_override: Option<&str>,
) -> Result<i32> {
    let service = service()?;
    let plugin_id = plugin_id(plugin)?;
    let host = match host_override {
        Some(host) => host.to_string(),
        None => match service.host_id() {
            Ok(host) => host,
            Err(error) => return Ok(report(error)),
        },
    };
    let project = match service.resolve_project(project) {
        Ok(project) => project,
        Err(error) => return Ok(report(error)),
    };
    let effects = match capabilities(capability) {
        Ok(effects) => effects,
        Err(error) => return Ok(report(error)),
    };
    match service.change_grant(&plugin_id, &project, &host, &effects) {
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

/// `plugins disable-project`
fn disable_project(plugin: &str, project: &str) -> Result<i32> {
    let service = service()?;
    let plugin_id = plugin_id(plugin)?;
    let project = match service.resolve_project(project) {
        Ok(project) => project,
        Err(error) => return Ok(report(error)),
    };
    match service.disable_project(&plugin_id, &project) {
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

/// `plugins activate`
///
/// 激活是一个**事务**：本命令只建立它。推进需要运行期控制面（停旧版本、
/// 起候选、健康检查、迁移、提交），那属于 daemon。这里必须明确报告
/// "尚未推进"，而不是假装激活完成。
fn activate(plugin: &str, version: &str) -> Result<i32> {
    let service = service()?;
    let plugin = plugin_id(plugin)?;
    match service.activate(&plugin, version) {
        Ok(activation) => {
            println!(
                "started activation transaction {} for {version} (generation {})",
                activation.transaction, activation.activation
            );
            if !activation.advanced {
                println!(
                    "note: the transaction is not advanced by this command; the daemon drives \
                     draining, migration and health checks"
                );
            }
            Ok(0)
        }
        Err(error) => Ok(report(error)),
    }
}

/// `plugins transaction`
fn transaction(plugin: &str) -> Result<i32> {
    let service = service()?;
    let plugin = plugin_id(plugin)?;
    match service.transaction(&plugin) {
        Ok(Some(view)) => {
            println!(
                "{}\t{}\t{}\tadmission={}",
                view.transaction,
                plugin.as_str(),
                view.phase,
                if view.admission_open {
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

/// `plugins cancel-transaction`
///
/// 真正的回滚动作需要 `RuntimeControl`；这里只做前置判断并如实说明。
fn cancel_transaction(plugin: &str) -> Result<i32> {
    let service = service()?;
    let plugin = plugin_id(plugin)?;
    match service.cancel_transaction(&plugin) {
        Ok(outcome) => {
            eprintln!(
                "transaction {} is at {} and can be cancelled, but undoing it needs the \
                 runtime control plane; the daemon performs the rollback",
                outcome.transaction, outcome.phase
            );
            Ok(1)
        }
        Err(error) => Ok(report(error)),
    }
}

/// `plugins rollback`
fn rollback(plugin: &str) -> Result<i32> {
    let service = service()?;
    let plugin = plugin_id(plugin)?;
    match service.transaction(&plugin) {
        Ok(Some(view)) if !view.rollbackable => Ok(report(PluginError::RecoveryRequired(format!(
            "transaction `{}` is at phase {}; rolling back after `projecting` would be a \
                 second commit, so it must be repaired instead",
            view.transaction, view.phase
        )))),
        Ok(_) => not_supported(
            "rollback",
            "needs RuntimeControl to stop the candidate and restore admission; that is the daemon's job",
        ),
        Err(error) => Ok(report(error)),
    }
}

/// `plugins uninstall`
fn uninstall_plugin(plugin: &str, remove_data: bool) -> Result<i32> {
    let service = service()?;
    let plugin = plugin_id(plugin)?;
    match service.uninstall(&plugin, remove_data) {
        Ok(outcome) => {
            println!(
                "uninstalled {} ({} versions, {} grants revoked)",
                outcome.plugin, outcome.versions_removed, outcome.grants_revoked
            );
            for data in &outcome.retained {
                match &data.path {
                    Some(path) => println!("kept {} data at {path} — {}", data.project, data.why),
                    None => println!("kept data for {} — {}", data.project, data.why),
                }
            }
            Ok(0)
        }
        Err(error) => Ok(report(error)),
    }
}

/// `plugins clear-data`
///
/// 独立于卸载的一次显式动作：要 `--yes`，还要点名项目。两样缺一样就不动
/// 磁盘——"把所有数据都删了"不该是一个可以被顺手敲出来的默认行为。
fn clear_data(plugin: &str, projects: &[String], yes: bool) -> Result<i32> {
    if !yes {
        eprintln!("error: clearing plugin data is irreversible; re-run with --yes to confirm");
        return Ok(1);
    }
    if projects.is_empty() {
        eprintln!("error: name at least one --project whose plugin data should be deleted");
        return Ok(1);
    }
    let service = service()?;
    let plugin = plugin_id(plugin)?;
    let mut outcome = match service.uninstall(&plugin, true) {
        Ok(outcome) => outcome,
        Err(error) => return Ok(report(error)),
    };
    let named: std::collections::BTreeSet<&str> = projects.iter().map(String::as_str).collect();
    outcome
        .retained
        .retain(|data| named.contains(data.project.as_str()));
    println!(
        "cleared data for {} in {} project(s)",
        outcome.plugin,
        named.len()
    );
    Ok(0)
}

/// `plugins gc`
fn gc() -> Result<i32> {
    let service = service()?;
    match service.gc() {
        Ok(outcome) => {
            for path in &outcome.removed {
                println!("removed {path}");
            }
            for kept in &outcome.kept {
                println!("kept {} — {}", kept.path, kept.why);
            }
            if outcome.removed.is_empty() && outcome.kept.is_empty() {
                println!("nothing to collect");
            }
            Ok(0)
        }
        Err(error) => Ok(report(error)),
    }
}

// ---------------------------------------------------------------------------
// 受管执行：留在 CLI 一侧（它要起前台子进程）
// ---------------------------------------------------------------------------

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
    let project = ProjectId::parse(project_handle)?;

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

// ---------------------------------------------------------------------------
// 维护屏障查询
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// 目录渠道的宿主侧输入
// ---------------------------------------------------------------------------

/// 目录文件的落点。由 `YI_AGENT_PLUGIN_CATALOG` 覆盖（测试与自托管用），
/// 缺省是实例根下的 `plugins/catalog.json`。
fn catalog_path() -> PathBuf {
    match std::env::var_os("YI_AGENT_PLUGIN_CATALOG") {
        Some(path) => PathBuf::from(path),
        None => yi_agent_store::paths::instance_home()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("plugins")
            .join("catalog.json"),
    }
}

/// 官方下载源。**不从目录里读**：目录只能指定路径，不能指定服务器的身份。
fn catalog_origin() -> Result<yi_agent_plugins::AllowedOrigin, PluginError> {
    let host = std::env::var("YI_AGENT_PLUGIN_ORIGIN").map_err(|_| {
        PluginError::NotSupported(
            "no official plugin origin is configured (set YI_AGENT_PLUGIN_ORIGIN); \
             refusing to download from an origin nobody vouched for"
                .to_string(),
        )
    })?;
    // 只接受一个裸主机名。带 scheme、端口或路径的值会被当成本地/内网源，
    // 而"官方源"的定义里没有这些形状——一个带端口的源足以把下载引到
    // 用户自己机器上的某个进程。
    if host.is_empty()
        || host.contains(':')
        || host.contains('/')
        || host.contains('@')
        || host.starts_with('-')
    {
        return Err(PluginError::Untrusted(format!(
            "`{host}` is not an official plugin origin; expected a bare https host name"
        )));
    }
    Ok(yi_agent_plugins::AllowedOrigin::official(host))
}

/// `plugin@version#sha256`。
fn parse_release_spec(spec: &str) -> Result<ReleaseId, PluginError> {
    let (identity, digest) = spec.rsplit_once('#').ok_or_else(|| {
        PluginError::InvalidManifest(
            "a release is `plugin@version#sha256`; the digest is not optional".to_string(),
        )
    })?;
    let (plugin, version) = identity.rsplit_once('@').ok_or_else(|| {
        PluginError::InvalidManifest(
            "a release is `plugin@version#sha256`; the version is not optional".to_string(),
        )
    })?;
    if digest.len() != 64 || !digest.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(PluginError::InvalidManifest(format!(
            "`{digest}` is not a sha256 digest"
        )));
    }
    Ok(ReleaseId {
        plugin: PluginId::parse(plugin)?,
        version: version.to_string(),
        target: service::host_target(),
        sha256: digest.to_ascii_lowercase(),
    })
}

fn load_catalog() -> Result<yi_agent_plugins::Catalog, PluginError> {
    let path = catalog_path();
    let bytes =
        std::fs::read(&path).map_err(|e| PluginError::Io(format!("{}: {e}", path.display())))?;
    // `Catalog` 刻意只实现 `Deserialize`（没有签名信封就谈不上"认证"），
    // 所以解析错误要在这里翻成协议里的错误种类，而不是让 serde 的原文
    // 漏到线协议上。
    serde_json::from_slice(&bytes).map_err(|e| {
        PluginError::InvalidManifest(format!("catalog at {} is not valid: {e}", path.display()))
    })
}

fn root_envelope() -> Result<Vec<u8>, PluginError> {
    let path = root_envelope_path();
    std::fs::read(&path).map_err(|e| {
        PluginError::Untrusted(format!(
            "no trust root available at {}: {e}",
            path.display()
        ))
    })
}

fn root_envelope_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("plugins/contracts/fixtures/root-v1.json")
}

/// 用 `curl` 下载：macOS 自带，不引入新的 Rust HTTP 依赖。
///
/// 每次调用都带上无进展超时（`--speed-limit`/`--speed-time`），这样"连接
/// 活着但不动"会被 curl 自己掐断，而不是永远挂着。重定向被关闭
/// （`--max-redirs 0`）：`yi_agent_plugins::download` 对每次跳转都要重新
/// 校验源，让 curl 自己跟就会绕过那一步。
#[derive(Default)]
struct CurlFetcher {
    /// 最近一次 `head` 的 URL。`read` 用它启动 GET。
    pending: Option<String>,
    /// 当前 GET 的子进程。
    ///
    /// `head` 只做 `-I`；GET 必须真的发出去，否则 `read` 永远读到 0 字节，
    /// 每一次真实下载都会以"大小与签名不符"失败——看起来像服务端问题，
    /// 实际是客户端根本没发 GET。
    current: Option<std::process::Child>,
}

impl CurlFetcher {
    /// 为最近一次 `head` 的 URL 启动 GET，并把 stdout 挂到 `current`。
    fn start_get(&mut self) -> Result<(), yi_agent_plugins::FetchError> {
        use yi_agent_plugins::FetchError;
        let Some(url) = self.pending.as_ref() else {
            return Err(FetchError::Transport(
                "read() was called before head(); there is no URL to fetch".to_string(),
            ));
        };
        let child = std::process::Command::new("curl")
            .args([
                "-sS",
                // 跟 `head` 用同一套策略：不让 curl 自己跟跳转，源校验由
                // `yi_agent_plugins::download` 逐跳完成。
                "--max-redirs",
                "0",
                // 缓冲读：一次写出 64 KiB 再进管道，否则每读一小段就要醒一次。
                "--no-buffer",
                "-f",
                "--max-time",
                "300",
                "--speed-limit",
                "1",
                "--speed-time",
                "30",
            ])
            .arg(url)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|error| FetchError::Transport(format!("could not run curl: {error}")))?;
        self.current = Some(child);
        Ok(())
    }
}

impl yi_agent_plugins::Fetcher for CurlFetcher {
    fn head(
        &mut self,
        url: &yi_agent_plugins::Url,
    ) -> Result<yi_agent_plugins::ResponseHead, yi_agent_plugins::FetchError> {
        use yi_agent_plugins::FetchError;
        let output = std::process::Command::new("curl")
            .args(["-sS", "--max-redirs", "0", "-I", "--max-time", "30"])
            .arg(format!("{}://{}{}", url.scheme, url.host, url.path))
            .output()
            .map_err(|error| FetchError::Transport(format!("could not run curl: {error}")))?;
        if !output.status.success() {
            return Err(FetchError::Transport(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ));
        }
        self.pending = Some(format!("{}://{}{}", url.scheme, url.host, url.path));
        let text = String::from_utf8_lossy(&output.stdout).to_string();
        let mut lines = text.lines();
        let status_line = lines.next().unwrap_or_default();
        let status = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse::<u16>().ok())
            .ok_or_else(|| FetchError::Transport("no status line".to_string()))?;
        let mut location = None;
        let mut content_length = None;
        for line in lines {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            match name.trim().to_ascii_lowercase().as_str() {
                "location" => location = Some(value.trim().to_string()),
                "content-length" => content_length = value.trim().parse::<u64>().ok(),
                _ => {}
            }
        }
        Ok(yi_agent_plugins::ResponseHead {
            status,
            location,
            content_length,
        })
    }

    fn read(&mut self, max_bytes: usize) -> Result<Option<Vec<u8>>, yi_agent_plugins::FetchError> {
        use std::io::Read;

        // 首次 `read` 才真正发 GET：`head` 只做一次 `-I`，此时还不知道
        // 是不是要下载（可能是重定向、4xx）。把 GET 放在这里，`download`
        // 决定要读的时候才付网络代价。
        if self.current.is_none() {
            self.start_get()?;
        }
        let Some(child) = self.current.as_mut() else {
            return Ok(None);
        };
        let Some(stdout) = child.stdout.as_mut() else {
            return Err(yi_agent_plugins::FetchError::Transport(
                "curl's stdout was already consumed".to_string(),
            ));
        };
        let mut buffer = vec![0u8; max_bytes];
        match stdout.read(&mut buffer) {
            Ok(0) => {
                // 流结束：等子进程退出并检查退出码。非零退出意味着传输被中断
                // （超时、连接重置），此时"读到 0 字节"不能当成正常结束。
                let status = child
                    .wait()
                    .map_err(|error| yi_agent_plugins::FetchError::Transport(error.to_string()))?;
                self.current = None;
                if !status.success() {
                    return Err(yi_agent_plugins::FetchError::Transport(format!(
                        "curl exited with {status} before the body completed"
                    )));
                }
                Ok(None)
            }
            Ok(n) => {
                buffer.truncate(n);
                Ok(Some(buffer))
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                self.current = None;
                Err(yi_agent_plugins::FetchError::Transport(error.to_string()))
            }
        }
    }
}

/// 单调时钟：从构造那一刻起算。
struct MonotonicClock {
    started: std::time::Instant,
}

impl MonotonicClock {
    fn new() -> MonotonicClock {
        MonotonicClock {
            started: std::time::Instant::now(),
        }
    }
}

impl yi_agent_plugins::Clock for MonotonicClock {
    fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

fn capabilities(raw: &[String]) -> Result<Vec<yi_agent_plugins::Effect>, PluginError> {
    raw.iter()
        .map(|value| service::parse_capability(value))
        .collect()
}

fn not_supported(method: &str, why: &str) -> Result<i32> {
    eprintln!("error [NotSupported]: {method} is not implemented yet: {why}");
    Ok(1)
}

use yi_agent_plugins::ProjectId;

#[cfg(test)]
mod curl_fetcher_tests {
    //! 下载器自身的成功路径。
    //!
    //! 这段测试存在的理由是一次真实缺陷：`CurlFetcher` 只做了 `curl -I`，
    //! `read` 依赖一个从未被赋值的 reader，于是**每一次**下载都读到 0 字节，
    //! 然后以"大小与签名不符"失败。错误信息指向服务端，真正的问题在客户端
    //! 根本没发 GET。只用 mock fetcher 的测试发现不了它，因为 mock 不区分
    //! "没发请求"和"发了但没内容"。

    use super::CurlFetcher;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use yi_agent_plugins::Fetcher;

    /// 起一个只服务一个请求的最小 HTTP 服务，返回端口与原始响应体。
    ///
    /// 不用 python3：这条测试要能在任何装了 curl 的环境里跑，而 `curl` 本身
    /// 已经是这个实现的依赖。
    fn serve_once(body: Vec<u8>) -> (String, Vec<u8>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        let expected = body.clone();
        std::thread::spawn(move || {
            // 两个请求：一个 `-I`（HEAD），一个 GET。
            for _ in 0..2 {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut request = [0u8; 4096];
                let _ = stream.read(&mut request);
                let head_only = request.starts_with(b"HEAD");
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                if !head_only {
                    let _ = stream.write_all(&body);
                }
                let _ = stream.flush();
            }
        });
        (format!("http://127.0.0.1:{port}/bundle.bin"), expected)
    }

    #[test]
    fn reads_the_body_after_head() {
        let body: Vec<u8> = (0..70_000u32).map(|i| (i % 251) as u8).collect();
        let (raw_url, expected) = serve_once(body);
        // `Url::parse` 刻意拒绝带端口的地址（官方下载没有这种形状）。
        // 这里要测的是**下载器**，所以直接构造 Url 值，绕过源校验那一层：
        // 两层各测各的，混在一起会让"下载器坏了"被"源校验拒绝"掩盖。
        let rest = raw_url.trim_start_matches("http://");
        let (authority, path) = rest.split_once('/').expect("authority/path");
        let url = yi_agent_plugins::Url {
            scheme: "http".to_string(),
            host: authority.to_string(),
            path: format!("/{path}"),
        };

        let mut fetcher = CurlFetcher::default();
        let head = fetcher.head(&url).expect("head");
        assert_eq!(head.status, 200);
        assert_eq!(head.content_length, Some(expected.len() as u64));

        let mut received = Vec::new();
        while let Some(chunk) = fetcher.read(64 * 1024).expect("read") {
            received.extend_from_slice(&chunk);
            assert!(received.len() <= expected.len(), "read past the body");
        }
        assert_eq!(
            received, expected,
            "the fetched bytes must match the server's"
        );
    }

    #[test]
    fn reading_without_a_head_is_an_error_not_an_empty_body() {
        let mut fetcher = CurlFetcher::default();
        let error = fetcher
            .read(1024)
            .expect_err("read without head must not pretend the stream ended");
        assert!(
            format!("{error}").contains("head"),
            "the error must name the missing step, got: {error}"
        );
    }
}
