//! `plugins/v1/*`：受管插件的管理 RPC。
//!
//! # 与 `plugins/*` 的关系
//!
//! `plugin/*` 与 `plugins/*`（不带版本段）是**旧的** supervisor 协议：它按项目
//! 装清单、开关、起守护。那些方法保留原样，因为它们服务于既有的旧安装。
//!
//! `plugins/v1/*` 是新的**受管**协议：装、授权、激活、事务、卸载都作用在实例
//! 根的受管记录上，走签名验证与生命周期事务。两者**不共享**授权：旧的
//! `plugin/install` 不因为"装过"就在新协议里变成"已授权"。
//!
//! # 它与 CLI 是同一份实现
//!
//! 全部委托给 [`PluginService`]。这里只做三件事：从 `params` 取出参数、调用、
//! 把结果编成 JSON。任何"再判断一次"的逻辑都属于服务层，不属于这里——两份
//! 判断就是两条规则，迟早分叉。
//!
//! # 内存分配与阻塞
//!
//! 管理动作会读写磁盘（有时还会下载）。它们都在 `spawn_blocking` 里跑：
//! 一次安装不该卡住整个 RPC 主循环。

use serde_json::{Value, json};

use crate::plugin_manager::service::{self, PluginService};
use yi_agent_plugins::{PluginError, PluginId, ReleaseId};

use crate::protocol::{RequestEnvelope, RequestId, RpcError};
use crate::rpc::{Flow, PluginChannelState, write_or_fatal};
use crate::server::{err_response, ok_response};

/// 方法前缀。旧协议的 `plugins/xxx` 不会被这里接住：`plugins/v1/...` 段数不同。
const PREFIX: &str = "plugins/v1/";

/// 本域是否处理这个方法。
pub(crate) fn handles(method: &str) -> bool {
    method.starts_with(PREFIX)
}

/// 处理一个 `plugins/v1/*` 请求。
pub(crate) async fn handle(
    state: &PluginChannelState,
    req: &RequestEnvelope,
    id: RequestId,
) -> anyhow::Result<Flow> {
    let method = req.method.as_str();
    let Some(verb) = method.strip_prefix(PREFIX) else {
        return Ok(Flow::Unknown);
    };

    // 受管管理方法一律要求 `Admin`。装包、授权、激活、卸载动的是**宿主**的
    // 状态；一个只读观察者能做这些，等于把"看板"和"装东西"混成同一权限。
    //
    // 门禁放在这里而不是 `server.rs` 的 `ADMIN_METHODS` 常量里：那个常量按
    // 方法名字面量硬编码，`plugins/v1/*` 是前缀域，逐个列举必然漏掉新方法，
    // 而漏掉的门禁不会有任何编译错误——只有一条越权路径。
    if state.client_scope < crate::protocol::Scope::Admin {
        write_or_fatal(
            state,
            id.clone(),
            err_response(
                id,
                RpcError::insufficient_scope(crate::protocol::Scope::Admin),
            ),
        )
        .await;
        return Ok(Flow::Continue);
    }

    let home = state.plugin_home.clone();
    let catalog_path = state.plugin_catalog.clone();
    let params = req.params.clone();
    let verb = verb.to_string();

    // 管理动作是阻塞的（读盘、验签、有时下载）。放进 blocking 池，别让一次
    // 安装卡住整个 RPC 循环。`verb` 与 `params` 都先取成 owned：`req` 只在
    // 函数体内有效，搬不进 `'static` 的任务。
    let outcome =
        tokio::task::spawn_blocking(move || dispatch(&home, &catalog_path, &verb, &params)).await;
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => {
            write_or_fatal(
                state,
                id.clone(),
                err_response(
                    id,
                    RpcError::internal(format!("plugin management task failed: {error}")),
                ),
            )
            .await;
            return Ok(Flow::Continue);
        }
    };

    match outcome {
        Ok(value) => write_or_fatal(state, id.clone(), ok_response(id, value)).await,
        Err(error) => {
            write_or_fatal(state, id.clone(), err_response(id, error_response(&error))).await
        }
    }
    Ok(Flow::Continue)
}

/// 一个协议错误 → 线协议错误。
///
/// `kind` 是稳定判据（客户端按它分支），`message` 只是给人看的。绝不把
/// Rust 的 `Debug` 输出当线协议。
fn error_response(error: &PluginError) -> RpcError {
    RpcError::plugin_query_error(kind_code(error), error.to_string())
}

/// `PluginError::kind()` 是 PascalCase；RPC 沿用既有的 snake_case `code`。
fn kind_code(error: &PluginError) -> &'static str {
    match error {
        PluginError::InvalidManifest(_) => "plugin_invalid_manifest",
        PluginError::Integrity(_) => "plugin_integrity",
        PluginError::Untrusted(_) => "plugin_untrusted",
        PluginError::Expired(_) => "plugin_expired",
        PluginError::Revoked(_) => "plugin_revoked",
        PluginError::Incompatible(_) => "plugin_incompatible",
        PluginError::NotSupported(_) => "plugin_not_supported",
        PluginError::PermissionDenied(_) => "plugin_permission_denied",
        PluginError::StaleBinding(_) => "plugin_stale_binding",
        PluginError::Dependency(_) => "plugin_dependency",
        PluginError::Conflict(_) => "plugin_conflict",
        PluginError::Draining(_) => "plugin_draining",
        PluginError::Busy(_) => "plugin_busy",
        PluginError::ResultUnknown(_) => "plugin_result_unknown",
        PluginError::RecoveryRequired(_) => "plugin_recovery_required",
        PluginError::NotFound(_) => "plugin_not_found",
        PluginError::Io(_) => "plugin_io",
    }
}

/// 真正的分发。纯同步，便于在 blocking 池里跑，也便于单测。
fn dispatch(
    home: &std::path::Path,
    catalog_path: &std::path::Path,
    verb: &str,
    params: &Value,
) -> Result<Value, PluginError> {
    let service = PluginService::open(home);
    match verb {
        "catalog" => catalog_at(catalog_path),
        "list" => list(&service),
        "install" => install(&service, params),
        "enableProject" => enable_project(&service, params),
        "changeGrant" => change_grant(&service, params),
        "disableProject" => disable_project(&service, params),
        "activate" => activate(&service, params),
        "rollback" => rollback(&service, params),
        "transaction" => transaction(&service, params),
        "cancelTransaction" => cancel_transaction(&service, params),
        "uninstall" => uninstall(&service, params),
        "clearData" => clear_data(&service, params),
        "gc" => gc(&service),
        // 探测已经落地；接管要用户逐个项目重新授权，作为独立动作随后接入。
        "inspectLegacy" => inspect_legacy(home, &service, params),
        "adoptLegacy" => adopt_legacy(home, &service, params),
        "assetRelease" => {
            let _ = &service;
            asset_release_at(catalog_path, params)
        }
        "operationResult" => Err(PluginError::NotSupported(
            "operation results are served by the operation ledger path, which lands with the \
             managed kanban migration (T23)"
                .to_string(),
        )),
        other => Err(PluginError::NotSupported(format!(
            "`plugins/v1/{other}` is not a management method"
        ))),
    }
}

// ---------------------------------------------------------------------------
// 各方法
// ---------------------------------------------------------------------------

/// 旧安装探测。**只读**：不执行旧程序、不删任何东西、不改任何登记。
///
/// 输入刻意是"用户点名的项目 + 实例已登记的常驻项目"，没有"给我扫一遍这台机器"
/// 的分支（设计 §8.1 / G3 已批的取舍）：全盘扫描既慢又会碰用户没授权的目录。
///
/// 请求可以带 `projects: [路径]` 显式补上没登记的项目；一个不存在的路径会被
/// 拒绝并原样报回，而**不**去找一个相近的存在路径顶替。
fn inspect_legacy(
    home: &std::path::Path,
    service: &PluginService,
    params: &Value,
) -> Result<Value, PluginError> {
    inspect_legacy_with_path(
        home,
        service,
        params,
        &std::env::var("PATH").unwrap_or_default(),
    )
}

/// 与 [`inspect_legacy`] 相同，但 `PATH` 由调用方给。
///
/// 测试必须能钉住 `PATH`：读真实环境会让"探测到什么"随开发机装了什么而变，
/// 而这条函数的核心承诺正是"只报我探到的、不猜"。
fn inspect_legacy_with_path(
    home: &std::path::Path,
    service: &PluginService,
    params: &Value,
    path: &str,
) -> Result<Value, PluginError> {
    let mut projects: Vec<std::path::PathBuf> = Vec::new();
    if let Some(requested) = params.get("projects") {
        let list = requested.as_array().ok_or_else(|| {
            PluginError::Conflict("`projects` must be an array of project paths".to_string())
        })?;
        for entry in list {
            let raw = entry.as_str().ok_or_else(|| {
                PluginError::Conflict("every entry of `projects` must be a string".to_string())
            })?;
            // 规范化才能让"同一项目的两种写法"被认成同一个；失败就把原样交出去，
            // 由探测把它记成 rejected input，而不是在这里猜。
            projects
                .push(std::fs::canonicalize(raw).unwrap_or_else(|_| std::path::PathBuf::from(raw)));
        }
    }
    // 已登记的常驻项目也是允许的探测来源。读不懂登记表就不追加，而不是报错——
    // 那会让"我想看看旧安装"因为一份坏登记表完全用不了。
    projects.extend(resident_project_paths(home));
    projects.sort();
    projects.dedup();

    let rules = legacy_rules(service, params);
    let report = yi_agent_plugins::legacy::probe(&yi_agent_plugins::legacy::Probe {
        projects,
        instance_dir: home,
        path,
        rules: &rules,
    });
    // 保持只读语义：`service` 只用来确认实例根打开过（与其它管理方法一致），
    // 探测本身不经过它。
    let _ = service;
    serde_json::to_value(report)
        .map_err(|error| PluginError::Io(format!("could not encode the report: {error}")))
}

/// 这个插件"旧安装长什么样"。
///
/// 优先读**已验证包**的清单（`data.legacy`）；读不到时回退到按插件 id 推导的
/// 最常见布局，并明确标注这是回退。回退值只用于"给用户看看这台机器上有什么"，
/// **不**用于接管：接管必须来自已安装、已验签的发布。
fn legacy_rules(service: &PluginService, params: &Value) -> yi_agent_plugins::legacy::LegacyRules {
    let fallback = || {
        params
            .get("plugin")
            .and_then(Value::as_str)
            .and_then(|raw| PluginId::parse(raw).ok())
            .map(|plugin| yi_agent_plugins::legacy::LegacyRules::for_plugin(&plugin))
            .unwrap_or_default()
    };
    let Some(plugin) = params
        .get("plugin")
        .and_then(Value::as_str)
        .and_then(|raw| PluginId::parse(raw).ok())
    else {
        return fallback();
    };
    let Some(selected) = service.selected_manifest(&plugin) else {
        return yi_agent_plugins::legacy::LegacyRules::for_plugin(&plugin);
    };
    match selected.data.legacy {
        Some(legacy) => legacy.rules(),
        None => yi_agent_plugins::legacy::LegacyRules::for_plugin(&plugin),
    }
}

/// 常驻登记里登记过的项目路径。坏表 → 空表（只是没有可追加的来源）。
fn resident_project_paths(home: &std::path::Path) -> Vec<std::path::PathBuf> {
    let path = home.join("resident-daemons.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    value
        .get("projects")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.get("project")?.as_str())
                .map(std::path::PathBuf::from)
                .collect()
        })
        .unwrap_or_default()
}

/// 接管一个旧安装（§8.2）。
///
/// 这里是**唯一**的接管入口：探测（`inspectLegacy`）只报告，接管要用户显式
/// 指定项目并重新授权。三个输入都必须显式给出，没有"帮我接管所有探到的"分支——
/// 那等于把一次扫描变成一次批量数据变更。
///
/// 顺序与失败语义都在 `yi_agent_supervisors::adopt` 里：先备份、再停旧服务、
/// 再建受管身份。这里只负责把 `params` 变成调用，并把 `PluginError` 归类。
fn adopt_legacy(
    home: &std::path::Path,
    service: &PluginService,
    params: &Value,
) -> Result<Value, PluginError> {
    let plugin = plugin_from_params(params)?;
    let raw_project = params
        .get("project")
        .and_then(Value::as_str)
        .filter(|project| !project.trim().is_empty())
        .ok_or_else(|| PluginError::Conflict("adoptLegacy needs a `project` path".to_string()))?;
    let capabilities = capabilities_from_params(params)?;

    // 规范化：项目身份要能跨"用户敲的路径"与"磁盘路径"两种写法对上。
    // 失败就原样交给下面的 `is_dir` 判断，由它报 NotFound——不在这里猜。
    let project = std::fs::canonicalize(raw_project)
        .unwrap_or_else(|_| std::path::PathBuf::from(raw_project));

    let release = service.selected_release(&plugin)?;
    let legacy_state_dir = project.join(".yi-agent").join(plugin.as_str());
    let request = yi_agent_supervisors::adopt::AdoptRequest {
        project: project.clone(),
        legacy_state_dir,
        capabilities,
        backup: true,
    };
    let owner = HostLegacyOwner {
        plugin: plugin.as_str(),
    };
    let ownership =
        yi_agent_plugins::ownership::ProjectOwnership::new(home.join("plugins").join("owners"));
    let adopted = yi_agent_supervisors::adopt::adopt(
        home,
        &plugin,
        &release,
        &request,
        &owner,
        &ownership,
        chrono::Local::now(),
    )?;
    serde_json::to_value(adopted)
        .map_err(|error| PluginError::Io(format!("could not encode the outcome: {error}")))
}

/// 支持的能力名 → `Effect`。未知名字报错，不静默丢弃。
fn effect_from_name(name: &str) -> Result<yi_agent_plugins::Effect, PluginError> {
    // 接受两种拼写：协议里的 `quota-write`（与清单一致，小写 kebab）与
    // Rust 变体名 `QuotaWrite`（UI 从清单里读到的就是这个名字）。同一个
    // `capabilities` 字段在不同入口要求不同写法，是最容易让用户踩空的地方。
    Ok(match name {
        "read" | "Read" => yi_agent_plugins::Effect::Read,
        "write" | "Write" => yi_agent_plugins::Effect::Write,
        "launch" | "Launch" => yi_agent_plugins::Effect::Launch,
        "quota-write" | "QuotaWrite" => yi_agent_plugins::Effect::QuotaWrite,
        "resident" | "Resident" => yi_agent_plugins::Effect::Resident,
        "maintenance" | "Maintenance" => yi_agent_plugins::Effect::Maintenance,
        other => {
            return Err(PluginError::Conflict(format!(
                "unknown capability `{other}`; expected read|write|launch|quota-write|resident"
            )));
        }
    })
}

fn plugin_from_params(params: &Value) -> Result<PluginId, PluginError> {
    let raw = params
        .get("plugin")
        .and_then(Value::as_str)
        .filter(|plugin| !plugin.trim().is_empty())
        .ok_or_else(|| PluginError::Conflict("adoptLegacy needs a `plugin` id".to_string()))?;
    PluginId::parse(raw)
}

/// 解析用户勾选的能力。
///
/// **不**用清单里的 `capabilities.required` 兜底：那会把"用户没勾"变成
/// "宿主替他勾了"，而接管正是要用户重新表态的时刻（§8.1）。
pub(crate) fn capabilities_from_params(
    params: &Value,
) -> Result<Vec<yi_agent_plugins::Effect>, PluginError> {
    let Some(raw) = params.get("capabilities") else {
        return Err(PluginError::PermissionDenied(
            "adoptLegacy needs an explicit `capabilities` list; the legacy switch is not a grant"
                .to_string(),
        ));
    };
    let list = raw.as_array().ok_or_else(|| {
        PluginError::Conflict("`capabilities` must be an array of effect names".to_string())
    })?;
    let mut effects = Vec::new();
    for entry in list {
        let name = entry.as_str().ok_or_else(|| {
            PluginError::Conflict("every entry of `capabilities` must be a string".to_string())
        })?;
        let effect = effect_from_name(name)?;
        if !effects.contains(&effect) {
            effects.push(effect);
        }
    }
    Ok(effects)
}

/// 宿主侧的外部动作。
///
/// # 存活判据
///
/// 插件自己的 `plugin.lock`（`<state_dir>/plugin.lock`）是 `flock`：持有者退出
/// （含 `SIGKILL`）内核就释放。**不用 PID 文件**——PID 会被回收，一个死进程
/// 留下的 PID 会被当成活着的，接管就永远做不了。
///
/// # 停止只针对这一个项目
///
/// 找到持锁那份 `supervisors/<plugin>.json` 指向的子进程并结束它；找不到
/// 明确的持有者就报阻塞，绝不按名字批量 `kill`——同名进程可能属于另一个实例、
/// 另一个项目，或用户手动跑的调试进程。
struct HostLegacyOwner<'a> {
    plugin: &'a str,
}

impl HostLegacyOwner<'_> {
    fn state_dir(&self, project: &std::path::Path) -> std::path::PathBuf {
        project.join(".yi-agent").join(self.plugin)
    }

    /// 旧插件的 `plugin.lock` 是否被持有。
    fn lock_held(&self, state_dir: &std::path::Path) -> bool {
        let path = state_dir.join("plugin.lock");
        if !path.exists() {
            return false;
        }
        let Ok(file) = std::fs::OpenOptions::new().write(true).open(&path) else {
            // 打不开不能证明"没人持有"，按仍持有处理。
            return true;
        };
        use std::os::unix::io::AsRawFd;
        let outcome = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if outcome == 0 {
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
            return false;
        }
        true
    }
}

impl yi_agent_supervisors::adopt::LegacyOwner for HostLegacyOwner<'_> {
    fn legacy_is_running(&self, project: &std::path::Path) -> Result<bool, PluginError> {
        Ok(self.lock_held(&self.state_dir(project)))
    }

    /// 停旧服务。
    ///
    /// **这里不发任何信号。** 三个理由，每一个都足以否掉"顺手 kill 掉"：
    ///
    /// 1. 我们只有"某个进程持有 `plugin.lock`"，没有它的 PID。按进程名
    ///    `pkill` 会连另一个实例、另一个项目、甚至用户手动起的调试进程一起
    ///    杀掉，而杀错进程是不可撤销的。
    /// 2. 旧进程由旧 supervisor 拉起，宿主重启它才是正确的停止方式；宿主
    ///    在这里的职责是**报告冲突**，不是替用户清理。
    /// 3. §8.2 明确要求"冲突时报告阻塞"。
    ///
    /// 因此这里只在"锁已经空了"时返回成功（此时没有东西可停），否则把
    /// 情况原样报出去，让用户自己关掉旧看板再重试。
    fn stop_legacy(&self, project: &std::path::Path) -> Result<(), PluginError> {
        let state_dir = self.state_dir(project);
        if self.lock_held(&state_dir) {
            return Err(PluginError::Busy(format!(
                "the legacy board for {} is still running (it holds {}); close it, then retry the \
                 adoption. The host does not kill processes it cannot identify.",
                project.display(),
                state_dir.join("plugin.lock").display()
            )));
        }
        Ok(())
    }

    /// 接管后的健康检查。
    ///
    /// 只做不依赖具体插件的检查：项目目录在、数据目录可读、`board.json`
    /// 是一个 JSON 对象（若存在）。**不**去连插件的 socket——那会让宿主
    /// 认识某个具体插件，而这一层必须保持通用。
    fn health_check(
        &self,
        project: &std::path::Path,
        _release: &ReleaseId,
    ) -> Result<(), PluginError> {
        if !project.is_dir() {
            return Err(PluginError::NotFound(format!(
                "{} is no longer a directory",
                project.display()
            )));
        }
        let state_dir = self.state_dir(project);
        if !state_dir.is_dir() {
            return Err(PluginError::Io(format!(
                "the managed state dir {} was not created",
                state_dir.display()
            )));
        }
        let board = state_dir.join("board.json");
        if board.is_file() {
            let bytes = std::fs::read(&board)
                .map_err(|error| PluginError::Io(format!("{}: {error}", board.display())))?;
            let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
                PluginError::Io(format!("{} is not valid JSON: {error}", board.display()))
            })?;
            if !value.is_object() {
                return Err(PluginError::Io(format!(
                    "{} is not a JSON object",
                    board.display()
                )));
            }
        }
        // 新 owner 必须拿得到项目锁；拿不到说明还有别的推进者在。
        if self.lock_held(&state_dir) {
            return Err(PluginError::Busy(format!(
                "{} is still locked after adoption",
                state_dir.join("plugin.lock").display()
            )));
        }
        Ok(())
    }

    /// 把项目投影到新代次。
    ///
    /// 受管插件从宿主注入的环境（`YI_AGENT_PLUGIN_BINDING`）推导自己的项目，
    /// 所以这里**不**写任何"项目 → 插件"的私有映射文件：写一份等于给宿主
    /// 增加一个必须与插件同步的磁盘契约，而两边分开演进时它会先烂掉。
    fn project(
        &self,
        _project: &std::path::Path,
        _release: &ReleaseId,
        _activation: u64,
    ) -> Result<(), PluginError> {
        Ok(())
    }
}

/// 目录内容。**只读**：不安装、不启用、不激活。
///
/// 目录条目不是信任凭据——它只说明"官方源上有这么个版本"。真正的授权仍来自
/// 每个包的签名。
fn catalog_at(path: &std::path::Path) -> Result<Value, PluginError> {
    // 目录的位置由**宿主装配**钉死，不接受请求传入路径：一个能指定"从哪读目录"
    // 的调用方，等于能指定"哪些版本算官方"。
    let bytes =
        std::fs::read(path).map_err(|e| PluginError::Io(format!("{}: {e}", path.display())))?;
    let catalog: yi_agent_plugins::Catalog = serde_json::from_slice(&bytes).map_err(|e| {
        PluginError::InvalidManifest(format!("catalog at {} is not valid: {e}", path.display()))
    })?;
    let entries: Vec<Value> = catalog
        .entries
        .iter()
        .map(|entry| {
            json!({
                "plugin": entry.release.plugin.as_str(),
                "version": entry.release.version,
                "target": entry.release.target,
                "sha256": entry.release.sha256,
                "sizeBytes": entry.size_bytes,
                "publishedAt": entry.published_at,
                "expiresAt": entry.expires_at,
            })
        })
        .collect();
    Ok(json!({ "serial": catalog.serial, "entries": entries }))
}

fn list(service: &PluginService) -> Result<Value, PluginError> {
    let rows: Vec<Value> = service
        .list()?
        .into_iter()
        .map(|summary| {
            json!({
                "plugin": summary.plugin,
                "selected": summary.selected,
                "installed": summary.installed,
                "projects": summary.projects,
                // 重启之后也要能看见：只在卸载的返回值里报"待清理"，
                // 进程一重启就没人知道还有残留了。
                "pendingCleanup": summary
                    .pending_cleanup
                    .iter()
                    .map(|entry| json!({ "project": entry.project, "remaining": entry.remaining }))
                    .collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(json!({ "plugins": rows }))
}

/// 安装。**不**启用、**不**激活、**不**写 supervisor 清单。
///
/// 二选一：`{release: {plugin, version, target, sha256}}` 或
/// `{bundlePath, metadataPath}`。两条路都走同一个验签与落盘。
fn install(service: &PluginService, params: &Value) -> Result<Value, PluginError> {
    let root = root_envelope()?;
    if let (Some(bundle), Some(metadata)) = (
        params.get("bundlePath").and_then(Value::as_str),
        params.get("metadataPath").and_then(Value::as_str),
    ) {
        let line = service.install_local(
            std::path::Path::new(bundle),
            std::path::Path::new(metadata),
            &root,
        )?;
        return Ok(json!({ "installed": true, "detail": line }));
    }

    if let Some(release) = params.get("release") {
        let spec = release
            .as_str()
            .map(str::to_string)
            .or_else(|| {
                Some(format!(
                    "{}@{}#{}",
                    release.get("plugin")?.as_str()?,
                    release.get("version")?.as_str()?,
                    release.get("sha256")?.as_str()?
                ))
            })
            .ok_or_else(|| {
                PluginError::InvalidManifest(
                    "`release` must be `plugin@version#sha256` or an object with those fields"
                        .to_string(),
                )
            })?;
        return Err(PluginError::NotSupported(format!(
            "downloading {spec} needs an injected fetcher; use the headless `yi-agent plugins \
             install --release {spec}` path until the desktop download coordinator lands"
        )));
    }

    Err(PluginError::InvalidManifest(
        "install needs either `release` or both `bundlePath` and `metadataPath`".to_string(),
    ))
}

fn enable_project(service: &PluginService, params: &Value) -> Result<Value, PluginError> {
    let plugin = plugin_arg(params)?;
    let project_path = string_arg(params, "project")?;
    let host = resolve_host(service, params)?;
    let effects = capabilities_arg(params)?;
    let project = service.resolve_project(&project_path)?;
    let generation = service.enable_project(&plugin, project.clone(), &host, &effects)?;
    Ok(json!({
        "plugin": plugin.as_str(),
        "project": project.as_str(),
        "generation": generation,
    }))
}

fn change_grant(service: &PluginService, params: &Value) -> Result<Value, PluginError> {
    let plugin = plugin_arg(params)?;
    let project_path = string_arg(params, "project")?;
    let host = resolve_host(service, params)?;
    let effects = capabilities_arg(params)?;
    let project = service.resolve_project(&project_path)?;
    let generation = service.change_grant(&plugin, &project, &host, &effects)?;
    Ok(json!({
        "plugin": plugin.as_str(),
        "project": project.as_str(),
        "generation": generation,
    }))
}

fn disable_project(service: &PluginService, params: &Value) -> Result<Value, PluginError> {
    let plugin = plugin_arg(params)?;
    let project_path = string_arg(params, "project")?;
    let project = service.resolve_project(&project_path)?;
    let generation = service.disable_project(&plugin, &project)?;
    Ok(json!({
        "plugin": plugin.as_str(),
        "project": project.as_str(),
        "generation": generation,
    }))
}

fn activate(service: &PluginService, params: &Value) -> Result<Value, PluginError> {
    let plugin = plugin_arg(params)?;
    let version = string_arg(params, "version")?;
    let activation = service.activate(&plugin, &version)?;
    Ok(json!({
        "transaction": activation.transaction,
        "activation": activation.activation,
        // 恒为 false：推进需要运行期控制面，那是 daemon 的事。客户端据此知道
        // "事务已建立，但还没走完"，而不是以为激活完成。
        "advanced": activation.advanced,
    }))
}

fn rollback(service: &PluginService, params: &Value) -> Result<Value, PluginError> {
    let plugin = plugin_arg(params)?;
    let view = service.transaction(&plugin)?;
    match view {
        Some(view) if !view.rollbackable => Err(PluginError::RecoveryRequired(format!(
            "transaction `{}` is at phase {}; rolling back after `projecting` would be a second \
             commit, so it must be repaired instead",
            view.transaction, view.phase
        ))),
        _ => Err(PluginError::NotSupported(
            "rolling back needs the runtime control plane (stop the candidate, restore \
             admission); the daemon drives it"
                .to_string(),
        )),
    }
}

fn transaction(service: &PluginService, params: &Value) -> Result<Value, PluginError> {
    let plugin = plugin_arg(params)?;
    match service.transaction(&plugin)? {
        Some(view) => Ok(json!({
            "plugin": plugin.as_str(),
            "transaction": view.transaction,
            "phase": view.phase,
            "admissionOpen": view.admission_open,
            "rollbackable": view.rollbackable,
        })),
        None => Ok(json!({
            "plugin": plugin.as_str(),
            "transaction": Value::Null,
        })),
    }
}

fn cancel_transaction(service: &PluginService, params: &Value) -> Result<Value, PluginError> {
    let plugin = plugin_arg(params)?;
    let outcome = service.cancel_transaction(&plugin)?;
    // 前置判断通过了，但真正的回滚需要运行期控制面。如实说"可以取消、但
    // 尚未执行"，而不是假装已经回滚。
    Err(PluginError::NotSupported(format!(
        "transaction {} is at {} and can be cancelled, but undoing it needs the runtime control \
         plane; the daemon performs the rollback",
        outcome.transaction, outcome.phase
    )))
}

fn uninstall(service: &PluginService, params: &Value) -> Result<Value, PluginError> {
    let plugin = plugin_arg(params)?;
    let remove_data = params
        .get("removeData")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let outcome = service.uninstall(&plugin, remove_data)?;
    Ok(json!({
        "plugin": outcome.plugin,
        "versionsRemoved": outcome.versions_removed,
        "grantsRevoked": outcome.grants_revoked,
        "retained": outcome
            .retained
            .iter()
            .map(|data| json!({ "project": data.project, "path": data.path, "why": data.why }))
            .collect::<Vec<_>>(),
        // 空 = 真的清干净了。非空 = 还有宿主入口留在盘上，界面必须显示
        // "待清理"而不是"已卸载"——那些残留会在项目重新可达后再拉起程序。
        "pending": outcome
            .pending
            .iter()
            .map(|entry| json!({ "project": entry.project, "remaining": entry.remaining }))
            .collect::<Vec<_>>(),
    }))
}

/// 清除数据：独立于卸载的一次显式动作。
///
/// `confirm` 与 `projects` 两样缺一样就不动磁盘。"把所有数据都删了"不该是
/// 一个可以被顺手敲出来的默认行为。
fn clear_data(service: &PluginService, params: &Value) -> Result<Value, PluginError> {
    let confirmed = params
        .get("confirm")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !confirmed {
        return Err(PluginError::PermissionDenied(
            "clearing plugin data is irreversible; pass `confirm: true`".to_string(),
        ));
    }
    let projects: Vec<String> = params
        .get("projects")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if projects.is_empty() {
        return Err(PluginError::PermissionDenied(
            "name at least one project whose plugin data should be deleted".to_string(),
        ));
    }
    let plugin = plugin_arg(params)?;
    // 名字必须传下去：先按全部授权项目删一遍再过滤输出，等于"看起来只删了
    // 点名的"，而盘上的其它项目数据已经没了。
    let outcome = service.clear_data(&plugin, &projects)?;
    Ok(json!({
        "plugin": outcome.plugin,
        "projects": projects,
        "pending": outcome
            .pending
            .iter()
            .map(|entry| json!({ "project": entry.project, "remaining": entry.remaining }))
            .collect::<Vec<_>>(),
    }))
}

fn gc(service: &PluginService) -> Result<Value, PluginError> {
    let outcome = service.gc()?;
    Ok(json!({
        "removed": outcome.removed,
        "kept": outcome
            .kept
            .iter()
            .map(|kept| json!({ "path": kept.path, "why": kept.why }))
            .collect::<Vec<_>>(),
    }))
}

// ---------------------------------------------------------------------------
// 参数
// ---------------------------------------------------------------------------

fn plugin_arg(params: &Value) -> Result<PluginId, PluginError> {
    PluginId::parse(string_arg(params, "plugin")?.as_str())
}

fn string_arg(params: &Value, name: &str) -> Result<String, PluginError> {
    params
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| PluginError::InvalidManifest(format!("missing or empty `{name}` parameter")))
}

/// 宿主身份：请求可覆盖（多实例诊断用），缺省取实例级 id。
///
/// 允许请求覆盖是**有意的**：它不是授权凭据（授权记录里同时写着 project 与
/// 代次），而多个宿主共用一个实例根时需要能分辨。不覆盖时取 supervisor 的
/// 实例 id，那是同一份配置。
fn resolve_host(service: &PluginService, params: &Value) -> Result<String, PluginError> {
    match params.get("hostId").and_then(Value::as_str) {
        Some(host) if !host.is_empty() => Ok(host.to_string()),
        _ => service.host_id(),
    }
}

fn capabilities_arg(params: &Value) -> Result<Vec<yi_agent_plugins::Effect>, PluginError> {
    let raw = params
        .get("capabilities")
        .and_then(Value::as_array)
        .map(|values| values.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    raw.into_iter().map(service::parse_capability).collect()
}

/// `assetRelease`：一条**已认证**发布的身份 + 受控传输句柄。
///
/// # 它故意不返回什么
///
/// 不返回下载 URL、不返回 cookie/token、不返回宿主凭据。页面的 iframe 拿不到
/// 任何可以直接 `fetch` 的东西——否则"桥是唯一业务通道"这句话就被一个字符串
/// 绕过了。调用方只能拿到一个句柄，由宿主自己按目录条目取字节。
///
/// # 身份从哪来
///
/// 目录条目（`Catalog::find`）只回答"官方目录说有这个版本"。所以返回的
/// `authenticated` 只有在**目录里确实有这条**且发布未过期时才为 true：
/// 未知发布是 `NotFound`，过期发布是 `Expired`，被撤回的由调用方在验签时挡。
///
/// 目录路径显式传入而不是读进程环境变量：同一进程里两个连接可以各有各的
/// 官方源（本地与远端），并行测试也不会互相改写。
fn asset_release_at(path: &std::path::Path, params: &Value) -> Result<Value, PluginError> {
    let plugin = plugin_arg(params)?;
    let version = string_arg(params, "version")?;
    let target = string_arg(params, "target")?;
    let sha256 = string_arg(params, "sha256")?;

    let bytes =
        std::fs::read(path).map_err(|e| PluginError::Io(format!("{}: {e}", path.display())))?;
    let catalog: yi_agent_plugins::Catalog = serde_json::from_slice(&bytes).map_err(|e| {
        PluginError::InvalidManifest(format!("catalog at {} is not valid: {e}", path.display()))
    })?;

    let release = yi_agent_plugins::ReleaseId {
        plugin,
        version,
        target,
        sha256,
    };
    let entry = catalog.find(&release).ok_or_else(|| {
        PluginError::NotFound(format!(
            "the catalog does not list {} {} for {}",
            release.plugin.as_str(),
            release.version,
            release.target
        ))
    })?;

    let now = service::unix_now();
    if entry.expires_at != 0 && entry.expires_at <= now {
        return Err(PluginError::Expired(format!(
            "the release expired at {} (now {now})",
            entry.expires_at
        )));
    }

    // 句柄只是"这四元组在目录里存在"的代号，携带不了任何权限：宿主下次取字节
    // 时还要重新查目录、重新验签。
    let handle = format!(
        "rel1_{}",
        hex8(&format!(
            "{}|{}|{}|{}|{}",
            release.plugin.as_str(),
            release.version,
            release.target,
            release.sha256,
            entry.serial
        ))
    );

    Ok(json!({
        "plugin": release.plugin.as_str(),
        "version": release.version,
        "target": release.target,
        "sha256": release.sha256,
        "sizeBytes": entry.size_bytes,
        "publishedAt": entry.published_at,
        "expiresAt": entry.expires_at,
        "serial": entry.serial,
        // 目录里查得到才算"认证"；签名仍由安装路径独立验证，这里不代替它。
        "authenticated": true,
        "transfer": { "handle": handle, "kind": "host-mediated" },
        // 明确写出来，防止有人日后"顺手"加上 URL。
        "directUrl": Value::Null,
        "credentials": Value::Null,
    }))
}

/// 短摘要：句柄不需要可逆，只需要稳定且不泄漏长度信息。
fn hex8(seed: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(seed.as_bytes());
    digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// 默认目录位置：进程环境变量优先，否则实例根下的 `plugins/catalog.json`。
///
/// 这个函数**只**给装配方（`server.rs` 的生产构造与测试 harness）用；
/// 分发路径不再调用它。
pub(crate) fn default_catalog_path() -> std::path::PathBuf {
    match std::env::var_os("YI_AGENT_PLUGIN_CATALOG") {
        Some(path) => std::path::PathBuf::from(path),
        None => yi_agent_store::paths::instance_home()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("plugins")
            .join("catalog.json"),
    }
}

fn root_envelope() -> Result<Vec<u8>, PluginError> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("plugins/contracts/fixtures/root-v1.json");
    std::fs::read(&path).map_err(|e| {
        PluginError::Untrusted(format!(
            "no trust root available at {}: {e}",
            path.display()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// `plugins/v1/*` 与旧的 `plugins/*` 不共用名称空间。
    #[test]
    fn only_versioned_methods_are_handled_here() {
        assert!(handles("plugins/v1/list"));
        // 旧的 `plugins/list`（supervisor 协议）必须留给旧的处理函数。
        assert!(!handles("plugins/list"));
        assert!(!handles("plugin/install"));
    }

    /// 每个错误都有稳定的 snake_case code。客户端按它分支。
    #[test]
    fn every_error_kind_maps_to_a_stable_code() {
        let cases = [
            (PluginError::Integrity("x".into()), "plugin_integrity"),
            (PluginError::Untrusted("x".into()), "plugin_untrusted"),
            (PluginError::NotFound("x".into()), "plugin_not_found"),
            (
                PluginError::RecoveryRequired("x".into()),
                "plugin_recovery_required",
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(kind_code(&error), expected);
        }
    }

    /// 下游任务的方法如实返回 NotSupported，而不是伪装成功。
    ///
    /// `assetRelease`（T20）、`inspectLegacy` 与 `adoptLegacy`（T23）已是真实实现，
    /// 故意不在表里：这条测试锁的是"**未实现**的要说明白"，不是"这些永远不实现"。
    #[test]
    fn downstream_methods_report_not_supported() {
        // 只剩一个方法；用单项列表而不是裸调用，加下一个时不必改结构。
        let verbs = ["operationResult"];
        for verb in verbs {
            let error = dispatch(
                std::path::Path::new("/nonexistent"),
                std::path::Path::new("/nonexistent/catalog.json"),
                verb,
                &json!({}),
            )
            .expect_err(verb);
            assert!(
                matches!(error, PluginError::NotSupported(_)),
                "{verb} must be NotSupported, got {error:?}"
            );
        }
    }

    /// `inspectLegacy` 是**只读**探测：给出可搬的旧看板，不改任何东西。
    #[test]
    fn inspect_legacy_reports_an_adoptable_board_without_touching_it() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join(".yi-agent");
        let project = root.path().join("proj");
        std::fs::create_dir_all(&home).unwrap();
        let state = project.join(".yi-agent/superpowers-kanban");
        std::fs::create_dir_all(&state).unwrap();
        let board = r#"{"cards":[],"next_order":0}"#;
        std::fs::write(state.join("board.json"), board).unwrap();

        // 明确指出是哪个插件：旧布局的自述来自那份插件的清单。插件不是身份
        // 判据而是查询参数——不给就无从知道该找哪些文件名。
        let report = inspect_legacy_with_path(
            &home,
            &PluginService::open(&home),
            &json!({
                "plugin": "superpowers-kanban",
                "projects": [project.to_string_lossy()],
            }),
            "",
        )
        .unwrap();

        let findings = report["findings"].as_array().unwrap();
        assert!(
            findings
                .iter()
                .any(|finding| finding["kind"] == "boardData"),
            "{report}"
        );
        // 探测读完之后文件原样还在。
        assert_eq!(
            std::fs::read_to_string(state.join("board.json")).unwrap(),
            board
        );
    }

    /// 不存在的项目路径被原样报回，不去找一个相近的存在路径顶替。
    #[test]
    fn inspect_legacy_reports_a_missing_project_instead_of_guessing_one() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join(".yi-agent");
        std::fs::create_dir_all(&home).unwrap();

        let report = inspect_legacy_with_path(
            &home,
            &PluginService::open(&home),
            &json!({ "projects": [root.path().join("typo").to_string_lossy()] }),
            "",
        )
        .unwrap();

        let rejected = report["rejectedInputs"].as_array().unwrap();
        assert_eq!(rejected.len(), 1, "{report}");
        assert!(rejected[0].as_str().unwrap().contains("typo"));
        assert!(
            report["findings"].as_array().unwrap().is_empty(),
            "{report}"
        );
    }

    /// `projects` 形状不对时明确报错，而不是静默当成"没有项目"。
    #[test]
    fn inspect_legacy_rejects_a_malformed_projects_param() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join(".yi-agent");
        std::fs::create_dir_all(&home).unwrap();

        let error = inspect_legacy_with_path(
            &home,
            &PluginService::open(&home),
            &json!({ "projects": "not-an-array" }),
            "",
        )
        .unwrap_err();
        assert!(error.to_string().contains("projects"), "{error}");
    }

    /// 没有目录时 `assetRelease` 明确报 Io，而不是返回一个空身份。
    #[test]
    fn asset_release_without_a_catalog_is_an_io_error() {
        let error = asset_release_at(
            std::path::Path::new("/nonexistent/catalog.json"),
            &json!({
                "plugin": "example-counter",
                "version": "1.0.0",
                "target": "darwin-aarch64",
                "sha256": "00",
            }),
        )
        .expect_err("no catalog");
        assert!(matches!(error, PluginError::Io(_)), "got {error:?}");
    }

    /// 目录里没有这条发布 → NotFound。绝不能返回一个"也许存在"的句柄。
    #[test]
    fn asset_release_for_an_unknown_release_is_not_found() {
        let dir = TempDir::new().expect("temp dir");
        let catalog = dir.path().join("catalog.json");
        std::fs::write(
            &catalog,
            serde_json::to_vec(&json!({ "serial": 1, "entries": [] })).unwrap(),
        )
        .unwrap();

        let error = asset_release_at(
            &catalog,
            &json!({
                "plugin": "example-counter",
                "version": "9.9.9",
                "target": "darwin-aarch64",
                "sha256": "00",
            }),
        )
        .expect_err("unknown release");
        assert!(matches!(error, PluginError::NotFound(_)), "got {error:?}");
    }

    /// 过期发布拒绝，即使目录里确实有这条。
    #[test]
    fn asset_release_refuses_an_expired_entry() {
        let dir = TempDir::new().expect("temp dir");
        let catalog = dir.path().join("catalog.json");
        std::fs::write(
            &catalog,
            serde_json::to_vec(&json!({
                "serial": 3,
                "entries": [{
                    "release": {
                        "plugin": "example-counter",
                        "version": "1.0.0",
                        "target": "darwin-aarch64",
                        "sha256": "ab".repeat(32),
                    },
                    "sizeBytes": 10,
                    "metadataPath": "/m.json",
                    "bundlePath": "/b.tar",
                    "publishedAt": 1,
                    "expiresAt": 2,
                    "serial": 3,
                }],
            }))
            .unwrap(),
        )
        .unwrap();

        let error = asset_release_at(
            &catalog,
            &json!({
                "plugin": "example-counter",
                "version": "1.0.0",
                "target": "darwin-aarch64",
                "sha256": "ab".repeat(32),
            }),
        )
        .expect_err("expired");
        assert!(matches!(error, PluginError::Expired(_)), "got {error:?}");
    }

    /// 清理数据要二次确认：没确认就什么都不做。
    #[test]
    fn clear_data_without_confirmation_refuses() {
        let service = PluginService::open(std::path::Path::new("/nonexistent"));
        let error =
            clear_data(&service, &json!({ "plugin": "example-counter" })).expect_err("must refuse");
        assert!(matches!(error, PluginError::PermissionDenied(_)));
    }

    #[test]
    fn clear_data_without_named_projects_refuses() {
        let service = PluginService::open(std::path::Path::new("/nonexistent"));
        let error = clear_data(
            &service,
            &json!({ "plugin": "example-counter", "confirm": true }),
        )
        .expect_err("must refuse");
        assert!(matches!(error, PluginError::PermissionDenied(_)));
    }

    /// 安装必须二选一，缺参数是明确错误而不是"什么都没装却说成功"。
    #[test]
    fn install_without_a_source_is_an_error() {
        let service = PluginService::open(std::path::Path::new("/nonexistent"));
        let error = install(&service, &json!({})).expect_err("must refuse");
        assert!(matches!(error, PluginError::InvalidManifest(_)));
    }

    #[test]
    fn a_missing_plugin_parameter_is_named_in_the_error() {
        let error = plugin_arg(&json!({})).expect_err("must refuse");
        assert!(error.to_string().contains("plugin"), "got {error}");
    }

    #[test]
    fn capabilities_are_parsed_and_maintenance_is_refused() {
        let effects = capabilities_arg(&json!({ "capabilities": ["read", "write"] }))
            .expect("read and write are user-facing");
        assert_eq!(
            effects,
            vec![
                yi_agent_plugins::Effect::Read,
                yi_agent_plugins::Effect::Write
            ]
        );
        let error = capabilities_arg(&json!({ "capabilities": ["maintenance"] }))
            .expect_err("maintenance is not user-grantable");
        assert!(matches!(error, PluginError::PermissionDenied(_)));
    }
}
